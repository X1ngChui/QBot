#!/usr/bin/env bash
#
# Deploy to the server, and prove it took effect.
#
# The reason this exists: `docker compose restart` does not rebuild. The Dockerfile COPYs
# qqbot/ and scripts/ into the image, so after a restart the new files sit on the server's
# disk while the container keeps running the old ones - container healthy, logs clean, no
# error anywhere, deployment silently lost.
#
# So: always --build, then compare the running code against what was just sent.
#
# config/ is a read-only mount rather than image content, so it needs no rebuild - but it
# is sent along anyway, because deciding which half changed is exactly the judgement call
# that got this wrong in the first place.
#
# Usage: scripts/deploy.sh
# Override QBOT_HOST / QBOT_KEY / QBOT_REMOTE for a different target.

set -euo pipefail

# "server" is an alias in ~/.ssh/config, which also names the key - so identity is
# the config's business unless QBOT_KEY forces one.
HOST=${QBOT_HOST:-server}
KEY=${QBOT_KEY:-}
REMOTE=${QBOT_REMOTE:-/opt/docker/qbot}
PYTHON=${PYTHON:-python}

cd "$(dirname "$0")/.."
SSH=(ssh -o StrictHostKeyChecking=accept-new "$HOST")
if [ -n "$KEY" ]; then SSH=(ssh -i "$KEY" -o StrictHostKeyChecking=accept-new "$HOST"); fi

# Everything the image is built from, plus the mounted config. Deliberately not .env or
# data/ - those live only on the server.
PAYLOAD=(qqbot scripts config sql bot.py requirements.txt Dockerfile docker-compose.yml .dockerignore)

# Directories are replaced, not merged. A plain `tar x` only overwrites, so a deleted
# module would stay on the server forever and get COPYed into every next image.
# Leave config/ untouched while building and checking: the running container has it
# mounted, and a failed gate must not change what that container would read on restart.
REPLACE=(qqbot scripts sql)
FILES=(bot.py requirements.txt Dockerfile docker-compose.yml .dockerignore)

echo "==> sending $(git rev-parse --short HEAD 2>/dev/null || echo 'working tree')"
# Land the whole archive first, and extract it beside the tree rather than over it:
# nothing of the old tree is removed until the new one has extracted whole, so a
# transfer or extract that dies leaves a server that still builds what it ran before.
tar czf - --exclude=__pycache__ "${PAYLOAD[@]}" | "${SSH[@]}" "cat > '$REMOTE/.deploy.tar.gz'"
"${SSH[@]}" "cd '$REMOTE' && rm -rf .deploy.new && mkdir .deploy.new && tar xzf .deploy.tar.gz -C .deploy.new && rm -f .deploy.tar.gz \
  && rm -rf ${REPLACE[*]} && for d in ${REPLACE[*]}; do mv .deploy.new/\$d .; done \
  && for f in ${FILES[*]}; do mv -f .deploy.new/\$f .; done"

echo "==> rebuilding"
# Keep one step back: tag the running image as :rollback before the build replaces
# it. A deploy that passes the fingerprint check but misbehaves at runtime can then
# be undone from the server alone - see README "Rollback" for the two commands. The
# container is found through compose, so the project name is not assumed.
"${SSH[@]}" "cd '$REMOTE' && cid=\$(docker compose ps -q bot 2>/dev/null); img=\$([ -n \"\$cid\" ] && docker inspect --format '{{.Image}}' \"\$cid\"); [ -n \"\$img\" ] && docker tag \"\$img\" qbot-bot:rollback || true"
"${SSH[@]}" "cd '$REMOTE' && docker compose build bot"

echo "==> checking database schema against staged configuration"
if ! "${SSH[@]}" "cd '$REMOTE' && docker compose run --rm --no-deps \
    -e CONFIG_DIR=/app/config.next \
    -v '$REMOTE/.deploy.new/config:/app/config.next:ro' \
    bot python scripts/check_schema.py"; then
    echo >&2
    echo "DEPLOY STOPPED: the database schema does not match this image and configuration." >&2
    echo "The running container and its mounted configuration were not replaced." >&2
    echo "Create and verify a fresh backup before any manual schema update; then" >&2
    echo "run scripts/check_schema.py with the staged configuration and deploy again." >&2
    exit 1
fi

# Stop the old image before changing any bytes inside its mounted config directory.
# A failed start restores the old config but does not blindly restart an image that
# might be incompatible with a manually updated database schema.
"${SSH[@]}" "cd '$REMOTE' && mkdir -p config && rm -rf .config.rollback && cp -a config .config.rollback && docker compose stop bot"
if ! "${SSH[@]}" "cd '$REMOTE' && find config -mindepth 1 -delete \
    && cp -a .deploy.new/config/. config/ && docker compose up -d --no-build bot"; then
    if ! "${SSH[@]}" "cd '$REMOTE' && docker compose stop bot \
      && find config -mindepth 1 -delete && cp -a .config.rollback/. config/"; then
        echo "DEPLOY FAILED: could not complete the rollback; inspect bot and config." >&2
        exit 1
    fi
    echo "DEPLOY FAILED: bot is stopped and old configuration restored." >&2
    echo "Check the schema before deciding whether the previous image can restart." >&2
    exit 1
fi
"${SSH[@]}" "cd '$REMOTE' && rm -rf .deploy.new"

echo "==> verifying"
# Compare both image code and the bind-mounted config, including prompt text.
want=$("$PYTHON" scripts/_fingerprint.py qqbot bot.py scripts config)
# tr strips the CR that comes back through ssh from a Windows terminal
got=$("${SSH[@]}" "cd '$REMOTE' && docker compose exec -T bot python /app/scripts/_fingerprint.py /app/qqbot /app/bot.py /app/scripts /app/config" | tr -d '\r')

if [ "$want" != "$got" ]; then
    echo >&2
    echo "DEPLOY FAILED: the running container is not the code that was just sent." >&2
    echo "  sent    $want" >&2
    echo "  running $got" >&2
    echo >&2
    echo "The build probably reused a cached layer, or an edit was never saved." >&2
    exit 1
fi

echo "ok - running $want"
"${SSH[@]}" "cd '$REMOTE' && docker compose logs --tail 3 bot"
