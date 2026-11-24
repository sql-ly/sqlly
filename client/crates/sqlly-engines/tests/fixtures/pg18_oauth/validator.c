/*
 * Test-only PostgreSQL 18 OAuth validator for tests/pg18_oauth_live.rs.
 * Accepts exactly the bearer token "good-token" and maps it to the role the
 * client asked for. Never install this anywhere but a throwaway container.
 */
#include "postgres.h"

#include "fmgr.h"
#include "libpq/oauth.h"

PG_MODULE_MAGIC;

static bool
validate_token(const ValidatorModuleState *state, const char *token,
			   const char *role, ValidatorModuleResult *result)
{
	result->authorized = strcmp(token, "good-token") == 0;
	result->authn_id = result->authorized ? pstrdup(role) : NULL;
	return true;
}

static const OAuthValidatorCallbacks callbacks = {
	PG_OAUTH_VALIDATOR_MAGIC,
	.validate_cb = validate_token,
};

const OAuthValidatorCallbacks *
_PG_oauth_validator_module_init(void)
{
	return &callbacks;
}
