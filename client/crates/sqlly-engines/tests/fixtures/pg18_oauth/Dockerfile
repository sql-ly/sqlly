# PostgreSQL 18 with OAuth sign-in for tests/pg18_oauth_live.rs.
#
#   docker build -t sqlly-pg18-oauth tests/fixtures/pg18_oauth
#   docker run -d --rm --name sqlly-pg18-oauth -p 127.0.0.1:55418:5432 \
#     -e POSTGRES_PASSWORD=postgres sqlly-pg18-oauth
#   SQLLY_PG18_OAUTH_PORT=55418 cargo test -p sqlly-engines \
#     --test pg18_oauth_live -- --ignored
#
# Role `alice` signs in with OAuth (the test validator accepts the token
# "good-token"); role `bob` uses a password, so OAuth is not offered to him.
FROM postgres:18

RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev libkrb5-dev openssl postgresql-server-dev-18 \
 && rm -rf /var/lib/apt/lists/*

COPY validator.c /tmp/validator.c
RUN gcc -fPIC -shared -I"$(pg_config --includedir-server)" \
      -o "$(pg_config --pkglibdir)/sqlly_test_validator.so" /tmp/validator.c

RUN mkdir -p /etc/sqlly-ssl \
 && openssl req -new -x509 -days 3650 -nodes -subj /CN=localhost \
      -out /etc/sqlly-ssl/server.crt -keyout /etc/sqlly-ssl/server.key \
 && chown -R postgres:postgres /etc/sqlly-ssl \
 && chmod 600 /etc/sqlly-ssl/server.key

COPY init.sh /docker-entrypoint-initdb.d/10-oauth.sh

CMD ["postgres", \
     "-c", "ssl=on", \
     "-c", "ssl_cert_file=/etc/sqlly-ssl/server.crt", \
     "-c", "ssl_key_file=/etc/sqlly-ssl/server.key", \
     "-c", "oauth_validator_libraries=sqlly_test_validator"]
