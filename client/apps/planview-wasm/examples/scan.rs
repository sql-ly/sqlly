//! Dev-only: what the viewer says about captured plan files.
//! `scan [--full] [--json] [--schema ddl.sql] [--query q.sql] plan-file...`
use sqlly_planview_wasm::{Options, analyze, decode_bytes};

fn main() {
    let (mut full, mut json) = (false, false);
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    let mut paths = Vec::new();
    let read = |p: Option<String>| p.and_then(|p| std::fs::read_to_string(p).ok());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--full" => full = true,
            "--json" => json = true,
            "--schema" => options.schema = read(args.next()).unwrap_or_default(),
            "--query" => options.query = read(args.next()).unwrap_or_default(),
            _ => paths.push(arg),
        }
    }
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = decode_bytes(&bytes);
        match analyze(&text, &options) {
            Err(e) => println!(
                "{path}\tERR {}",
                e.message.chars().take(90).collect::<String>()
            ),
            Ok(doc) => {
                if json {
                    println!("{doc}");
                    continue;
                }
                let mut parts = Vec::new();
                if full {
                    println!("schema: {}", doc["schema"]);
                    println!("aliases resolved: {}", doc["aliasesResolved"]);
                }
                for s in doc["statements"].as_array().into_iter().flatten() {
                    let findings = s["findings"].as_array().cloned().unwrap_or_default();
                    parts.push(format!(
                        "[n={} t={} {}]",
                        s["facts"]["operatorCount"],
                        s["facts"]["totalTimeText"],
                        findings
                            .iter()
                            .map(|f| format!(
                                "{}:{}{}",
                                f["kind"].as_str().unwrap_or(""),
                                f["severity"].as_str().unwrap_or(""),
                                if f["usedSchema"] == true { "*" } else { "" }
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                    if full {
                        println!("STORY: {}", s["story"]["text"].as_str().unwrap_or(""));
                        for f in &findings {
                            println!(
                                "- [{} {} conf={} schema={}] {} | {}\n    why: {}\n    do: {}\n    sql: {}",
                                f["kind"],
                                f["severity"],
                                f["confidence"],
                                f["usedSchema"],
                                f["impactText"],
                                f["summary"],
                                f["why"],
                                f["action"],
                                f["actionSql"]
                            );
                            println!("    alts: {}", f["alternatives"]);
                            println!("    evidence: {}", f["evidence"]);
                            println!("    notes: {}", f["notes"]);
                            println!(
                                "    learn: {}",
                                f["learn"]
                                    .as_array()
                                    .map(|l| l
                                        .iter()
                                        .map(|x| x["path"].as_str().unwrap_or("").to_string())
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default()
                            );
                        }
                        println!("ruledOut: {}", s["ruledOut"]);
                    }
                }
                println!("{path}\t{}\t{}", doc["sourceFormat"], parts.join(" "));
            }
        }
    }
}
