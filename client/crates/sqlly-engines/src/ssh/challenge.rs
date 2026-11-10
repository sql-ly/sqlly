//! SSH keyboard-interactive sign-in: the second step of servers configured
//! with `AuthenticationMethods publickey,keyboard-interactive` (a one-time
//! code from an authenticator app, a hardware token, a PAM question).
//!
//! The first method (key file, agent, or password) comes back as a partial
//! success naming `keyboard-interactive` as what is left. The server then
//! sends one or more rounds of questions; each round goes to the installed
//! [`SshChallengeResponder`] (the desktop app's dialog), and the answers go
//! straight back to the server. Answers are never stored: they live only in
//! the round that sent them.

use std::sync::{Arc, OnceLock};

use russh::MethodKind;
use russh::client::{AuthResult, KeyboardInteractiveAuthResponse};
use sqlly_core::{DisplayMessage, StudioError, StudioResult};

use crate::ssh::known_hosts::PromptClock;

/// One round of questions from the SSH server, as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshChallengePrompt {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// The server's title for this round (often empty).
    pub name: String,
    /// The server's instructions for this round (often empty).
    pub instructions: String,
    pub fields: Vec<SshChallengeField>,
}

/// One question in a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshChallengeField {
    /// The server's own wording, e.g. `Verification code: `.
    pub prompt: String,
    /// Whether the answer may be shown as it is typed; `false` means mask it.
    pub echo: bool,
}

/// Answers one round: `Some(answers)` in field order, or `None` when the
/// user cancelled. Called on a blocking thread, so it may wait for a dialog.
pub type SshChallengeResponder =
    Arc<dyn Fn(SshChallengePrompt) -> Option<Vec<String>> + Send + Sync>;

static RESPONDER: OnceLock<SshChallengeResponder> = OnceLock::new();

/// Installs the process-wide responder (the desktop app's dialog). Without
/// one, a server that asks for a code fails with a message saying so.
pub fn install_ssh_challenge_responder(responder: SshChallengeResponder) {
    let _ = RESPONDER.set(responder);
}

/// A server that keeps asking after this many rounds is given up on.
pub(crate) const MAX_ROUNDS: usize = 8;

/// What to do after the first sign-in method answered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterFirstMethod {
    Done,
    /// Accepted, but the server wants keyboard-interactive as well.
    KeyboardInteractive,
    Failed,
}

pub(crate) fn after_first_method(result: &AuthResult) -> AfterFirstMethod {
    match result {
        AuthResult::Success => AfterFirstMethod::Done,
        AuthResult::Failure {
            remaining_methods,
            partial_success: true,
        } if remaining_methods.contains(&MethodKind::KeyboardInteractive) => {
            AfterFirstMethod::KeyboardInteractive
        }
        AuthResult::Failure { .. } => AfterFirstMethod::Failed,
    }
}

/// The server side of the exchange, so the loop can be driven by a fake.
pub(crate) trait KeyboardInteractiveExchange {
    async fn start(&mut self, user: &str) -> Result<KeyboardInteractiveAuthResponse, russh::Error>;
    async fn respond(
        &mut self,
        answers: Vec<String>,
    ) -> Result<KeyboardInteractiveAuthResponse, russh::Error>;
}

impl<H: russh::client::Handler> KeyboardInteractiveExchange for russh::client::Handle<H> {
    async fn start(&mut self, user: &str) -> Result<KeyboardInteractiveAuthResponse, russh::Error> {
        self.authenticate_keyboard_interactive_start(user, None::<String>)
            .await
    }

    async fn respond(
        &mut self,
        answers: Vec<String>,
    ) -> Result<KeyboardInteractiveAuthResponse, russh::Error> {
        self.authenticate_keyboard_interactive_respond(answers)
            .await
    }
}

/// Where the user's answers come from, so the loop can be tested without a
/// dialog. `Ok(None)` is a cancel.
pub(crate) trait ChallengeAsker {
    async fn ask(&mut self, prompt: SshChallengePrompt) -> StudioResult<Option<Vec<String>>>;
}

/// Runs keyboard-interactive to the end: every round with questions goes to
/// `asker`, a round without questions is answered empty (OpenSSH sends one
/// after the last code), and the server's verdict decides.
pub(crate) async fn run_keyboard_interactive(
    exchange: &mut impl KeyboardInteractiveExchange,
    asker: &mut impl ChallengeAsker,
    host: &str,
    port: u16,
    username: &str,
) -> StudioResult<()> {
    let mut reply = exchange
        .start(username)
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))?;
    for _ in 0..MAX_ROUNDS {
        let (name, instructions, prompts) = match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                return Err(rejected(host, port));
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => (name, instructions, prompts),
        };
        let answers = if prompts.is_empty() {
            Vec::new()
        } else {
            let expected = prompts.len();
            let prompt = SshChallengePrompt {
                host: host.to_string(),
                port,
                username: username.to_string(),
                name,
                instructions,
                fields: prompts
                    .into_iter()
                    .map(|p| SshChallengeField {
                        prompt: p.prompt,
                        echo: p.echo,
                    })
                    .collect(),
            };
            match asker.ask(prompt).await? {
                Some(answers) if answers.len() == expected => answers,
                Some(_) => return Err(rejected(host, port)),
                None => return Err(cancelled(host, port)),
            }
        };
        reply = exchange
            .respond(answers)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
    }
    Err(StudioError::AuthenticationMessage(
        DisplayMessage::new(
            "services.errors.ssh_challenge_too_many_rounds",
            format!(
                "The SSH server {host}:{port} kept asking for more answers, so SQLLY stopped after {MAX_ROUNDS} rounds."
            ),
        )
        .with_arg("host", host)
        .with_arg("port", port.to_string())
        .with_arg("count", MAX_ROUNDS.to_string()),
    ))
}

fn rejected(host: &str, port: u16) -> StudioError {
    StudioError::AuthenticationMessage(
        DisplayMessage::new(
            "services.errors.ssh_challenge_rejected",
            format!(
                "The SSH server {host}:{port} did not accept the verification code. Check the code and connect again."
            ),
        )
        .with_arg("host", host)
        .with_arg("port", port.to_string()),
    )
}

fn cancelled(host: &str, port: u16) -> StudioError {
    StudioError::AuthenticationMessage(
        DisplayMessage::new(
            "services.errors.ssh_challenge_cancelled",
            format!("SSH sign-in to {host}:{port} was cancelled."),
        )
        .with_arg("host", host)
        .with_arg("port", port.to_string()),
    )
}

fn unavailable(host: &str, port: u16) -> StudioError {
    StudioError::AuthenticationMessage(
        DisplayMessage::new(
            "services.errors.ssh_challenge_unavailable",
            format!(
                "The SSH server {host}:{port} asks for a verification code, which can only be entered in the SQLLY desktop app."
            ),
        )
        .with_arg("host", host)
        .with_arg("port", port.to_string()),
    )
}

/// Asks the installed responder on a blocking thread, with the connect's
/// prompt clock running so the time the user spends finding the code is not
/// charged to the SSH connect timeout (the host-key dialog works the same
/// way, see `tunnel::timeout_unless_prompting`).
pub(crate) struct ResponderAsker {
    pub(crate) clock: Arc<PromptClock>,
    pub(crate) responder: Option<SshChallengeResponder>,
}

impl ResponderAsker {
    pub(crate) fn installed(clock: Arc<PromptClock>) -> Self {
        Self {
            clock,
            responder: RESPONDER.get().cloned(),
        }
    }
}

impl ChallengeAsker for ResponderAsker {
    async fn ask(&mut self, prompt: SshChallengePrompt) -> StudioResult<Option<Vec<String>>> {
        let (host, port) = (prompt.host.clone(), prompt.port);
        let Some(responder) = self.responder.clone() else {
            return Err(unavailable(&host, port));
        };
        // Same handshake with the timeout as the host-key check: claim the
        // connect before asking, so a timeout that already gave up wins and
        // no dialog opens for a dead connect.
        if !self.clock.announce_open() {
            self.clock.withdraw_announcement();
            return Err(cancelled(&host, port));
        }
        let open = self.clock.open();
        let answer = tokio::task::spawn_blocking(move || responder(prompt)).await;
        drop(open);
        if self.clock.is_abandoned() {
            return Err(cancelled(&host, port));
        }
        answer.map_err(|e| StudioError::Connection(format!("SSH sign-in prompt stopped: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::MethodSet;
    use russh::client::Prompt;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// A scripted server: each `start`/`respond` pops the next reply and
    /// records what the client sent.
    struct FakeServer {
        replies: VecDeque<KeyboardInteractiveAuthResponse>,
        started_as: Option<String>,
        answers: Vec<Vec<String>>,
    }

    impl FakeServer {
        fn new(replies: Vec<KeyboardInteractiveAuthResponse>) -> Self {
            Self {
                replies: replies.into(),
                started_as: None,
                answers: Vec::new(),
            }
        }
    }

    impl KeyboardInteractiveExchange for FakeServer {
        async fn start(
            &mut self,
            user: &str,
        ) -> Result<KeyboardInteractiveAuthResponse, russh::Error> {
            self.started_as = Some(user.to_string());
            Ok(self.replies.pop_front().expect("scripted reply"))
        }

        async fn respond(
            &mut self,
            answers: Vec<String>,
        ) -> Result<KeyboardInteractiveAuthResponse, russh::Error> {
            self.answers.push(answers);
            Ok(self.replies.pop_front().expect("scripted reply"))
        }
    }

    struct ScriptedUser {
        answers: VecDeque<Option<Vec<String>>>,
        seen: Vec<SshChallengePrompt>,
    }

    impl ScriptedUser {
        fn new(answers: Vec<Option<Vec<String>>>) -> Self {
            Self {
                answers: answers.into(),
                seen: Vec::new(),
            }
        }
    }

    impl ChallengeAsker for ScriptedUser {
        async fn ask(&mut self, prompt: SshChallengePrompt) -> StudioResult<Option<Vec<String>>> {
            self.seen.push(prompt);
            Ok(self.answers.pop_front().expect("scripted answer"))
        }
    }

    fn info(prompts: &[(&str, bool)]) -> KeyboardInteractiveAuthResponse {
        KeyboardInteractiveAuthResponse::InfoRequest {
            name: "Duo".into(),
            instructions: "Enter the code from your app".into(),
            prompts: prompts
                .iter()
                .map(|(prompt, echo)| Prompt {
                    prompt: prompt.to_string(),
                    echo: *echo,
                })
                .collect(),
        }
    }

    fn failure() -> KeyboardInteractiveAuthResponse {
        KeyboardInteractiveAuthResponse::Failure {
            remaining_methods: MethodSet::empty(),
            partial_success: false,
        }
    }

    fn key(err: &StudioError) -> Option<&'static str> {
        match err {
            StudioError::AuthenticationMessage(m) => m.key(),
            _ => None,
        }
    }

    #[test]
    fn a_partial_publickey_success_that_names_keyboard_interactive_continues() {
        let methods: MethodSet = (&[MethodKind::KeyboardInteractive][..]).into();
        let partial = AuthResult::Failure {
            remaining_methods: methods.clone(),
            partial_success: true,
        };
        assert_eq!(
            after_first_method(&partial),
            AfterFirstMethod::KeyboardInteractive
        );
        assert_eq!(
            after_first_method(&AuthResult::Success),
            AfterFirstMethod::Done
        );
        // A plain rejection, even one listing keyboard-interactive, is a
        // failed first method: the key or password was wrong.
        let rejected = AuthResult::Failure {
            remaining_methods: methods,
            partial_success: false,
        };
        assert_eq!(after_first_method(&rejected), AfterFirstMethod::Failed);
        // Partial success that wants something SQLLY cannot do here.
        let other = AuthResult::Failure {
            remaining_methods: (&[MethodKind::HostBased][..]).into(),
            partial_success: true,
        };
        assert_eq!(after_first_method(&other), AfterFirstMethod::Failed);
    }

    #[tokio::test]
    async fn the_code_is_asked_for_and_sent_once() {
        let mut server = FakeServer::new(vec![
            info(&[("Verification code: ", false)]),
            // OpenSSH sends an empty round after the last answer.
            info(&[]),
            KeyboardInteractiveAuthResponse::Success,
        ]);
        let mut user = ScriptedUser::new(vec![Some(vec!["123456".into()])]);
        run_keyboard_interactive(&mut server, &mut user, "bastion", 22, "deploy")
            .await
            .unwrap();
        assert_eq!(server.started_as.as_deref(), Some("deploy"));
        assert_eq!(
            server.answers,
            vec![vec!["123456".to_string()], Vec::<String>::new()]
        );
        assert_eq!(user.seen.len(), 1, "the empty round is not shown");
        let prompt = &user.seen[0];
        assert_eq!(prompt.host, "bastion");
        assert_eq!(prompt.username, "deploy");
        assert_eq!(prompt.name, "Duo");
        assert_eq!(prompt.instructions, "Enter the code from your app");
        assert_eq!(
            prompt.fields,
            vec![SshChallengeField {
                prompt: "Verification code: ".into(),
                echo: false
            }]
        );
    }

    #[tokio::test]
    async fn several_rounds_and_several_questions_are_answered_in_order() {
        let mut server = FakeServer::new(vec![
            info(&[("Password: ", false), ("Token serial: ", true)]),
            info(&[("OTP: ", false)]),
            KeyboardInteractiveAuthResponse::Success,
        ]);
        let mut user = ScriptedUser::new(vec![
            Some(vec!["pw".into(), "T-1".into()]),
            Some(vec!["999000".into()]),
        ]);
        run_keyboard_interactive(&mut server, &mut user, "h", 2222, "u")
            .await
            .unwrap();
        assert_eq!(
            server.answers,
            vec![
                vec!["pw".to_string(), "T-1".to_string()],
                vec!["999000".to_string()]
            ]
        );
        assert!(user.seen[0].fields[1].echo);
    }

    #[tokio::test]
    async fn a_wrong_code_fails_with_the_rejected_message() {
        let mut server = FakeServer::new(vec![info(&[("Code: ", false)]), failure()]);
        let mut user = ScriptedUser::new(vec![Some(vec!["000000".into()])]);
        let err = run_keyboard_interactive(&mut server, &mut user, "h", 22, "u")
            .await
            .unwrap_err();
        assert_eq!(key(&err), Some("services.errors.ssh_challenge_rejected"));
    }

    #[tokio::test]
    async fn cancelling_sends_nothing_to_the_server() {
        let mut server = FakeServer::new(vec![info(&[("Code: ", false)])]);
        let mut user = ScriptedUser::new(vec![None]);
        let err = run_keyboard_interactive(&mut server, &mut user, "h", 22, "u")
            .await
            .unwrap_err();
        assert_eq!(key(&err), Some("services.errors.ssh_challenge_cancelled"));
        assert!(server.answers.is_empty());
    }

    #[tokio::test]
    async fn a_server_that_never_stops_asking_is_given_up_on() {
        let mut replies = vec![info(&[])];
        replies.extend((0..MAX_ROUNDS).map(|_| info(&[])));
        let mut server = FakeServer::new(replies);
        let mut user = ScriptedUser::new(Vec::new());
        let err = run_keyboard_interactive(&mut server, &mut user, "h", 22, "u")
            .await
            .unwrap_err();
        assert_eq!(
            key(&err),
            Some("services.errors.ssh_challenge_too_many_rounds")
        );
    }

    #[tokio::test]
    async fn without_a_responder_the_error_says_to_use_the_desktop_app() {
        let mut asker = ResponderAsker {
            clock: Arc::new(PromptClock::default()),
            responder: None,
        };
        let mut server = FakeServer::new(vec![info(&[("Code: ", false)])]);
        let err = run_keyboard_interactive(&mut server, &mut asker, "h", 22, "u")
            .await
            .unwrap_err();
        assert_eq!(key(&err), Some("services.errors.ssh_challenge_unavailable"));
    }

    /// The time spent answering is banked on the prompt clock (so the SSH
    /// connect timeout does not count it), and the clock is closed after.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_answer_wait_is_banked_on_the_prompt_clock() {
        let clock = Arc::new(PromptClock::default());
        let responder: SshChallengeResponder = Arc::new(|prompt| {
            std::thread::sleep(Duration::from_millis(50));
            Some(vec![format!("code-for-{}", prompt.username)])
        });
        let mut asker = ResponderAsker {
            clock: clock.clone(),
            responder: Some(responder),
        };
        let mut server = FakeServer::new(vec![
            info(&[("Code: ", false)]),
            KeyboardInteractiveAuthResponse::Success,
        ]);
        run_keyboard_interactive(&mut server, &mut asker, "h", 22, "ops")
            .await
            .unwrap();
        assert_eq!(server.answers, vec![vec!["code-for-ops".to_string()]]);
        assert!(
            clock.total() >= Duration::from_millis(50),
            "{:?}",
            clock.total()
        );
        assert!(!clock.is_open());
    }

    /// A connect the timeout already gave up on never opens the prompt.
    #[tokio::test]
    async fn an_abandoned_connect_is_not_asked_for_a_code() {
        let clock = Arc::new(PromptClock::default());
        assert!(clock.try_abandon());
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = asked.clone();
        let responder: SshChallengeResponder = Arc::new(move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(vec!["x".into()])
        });
        let mut asker = ResponderAsker {
            clock,
            responder: Some(responder),
        };
        let mut server = FakeServer::new(vec![info(&[("Code: ", false)])]);
        let err = run_keyboard_interactive(&mut server, &mut asker, "h", 22, "u")
            .await
            .unwrap_err();
        assert_eq!(key(&err), Some("services.errors.ssh_challenge_cancelled"));
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(server.answers.is_empty());
    }
}
