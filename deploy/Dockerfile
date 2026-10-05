#
# Build context is the repository root:  docker build -f deploy/Dockerfile .
#
# What is baked into the image (read-only, versioned with the code):
#   /usr/local/bin/qbot              the binary, with the default configuration compiled in
#   pg_dump, pg_restore (17)         used by the nightly backup
#   /usr/share/qbot/defaults.toml    the same defaults, as a reference for operators
# What is supplied at runtime (never baked in):
#   /etc/qbot        configuration  (mount read-only: config.toml, conf.d/, personas/, locales/)
#   /var/lib/qbot    persistent state (a volume: models, backups)
#   /run/secrets     credentials     (mounted secret files, or environment variables)

ARG RUST_VERSION=1.99
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Compiled into the binary: the English and zh-CN catalogs and the instruction templates.
COPY locales ./locales
COPY prompts ./prompts
RUN cargo build --release --locked -p qbot-app

FROM debian:bookworm-slim AS runtime
# pg_dump and pg_restore for backups, at the server's major version (a dump tool older than the
# server refuses to run), from the PostgreSQL project's own repository.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
 && install -d /usr/share/postgresql-common/pgdg \
 && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
 && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" > /etc/apt/sources.list.d/pgdg.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends postgresql-client-17 \
 && apt-get purge -y --auto-remove curl gnupg \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin qbot \
 && mkdir -p /etc/qbot /var/lib/qbot /usr/share/qbot \
 && chown qbot:qbot /var/lib/qbot

COPY --from=build /src/target/release/qbot /usr/local/bin/qbot
COPY crates/qbot-config/defaults.toml /usr/share/qbot/defaults.toml

# Where the process looks for things. Override with -e or in the compose file; every path is
# also a mount point, so the layout never needs restructuring.
ENV QBOT_CONFIG_DIR=/etc/qbot \
    QBOT_DATA_DIR=/var/lib/qbot \
    QBOT_SECRETS_DIR=/run/secrets

VOLUME ["/var/lib/qbot"]
USER qbot
ENTRYPOINT ["qbot"]
# Serve the platform connection. `check-config` validates a deployment without starting it.
CMD ["run"]
