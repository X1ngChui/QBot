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
# module would stay on the server forever and get COPYed into every next image - and
# a deleted module that something can still import is a very quiet bug.
# config/ is NOT in this list: it is bind-mounted into the running container, and
# rm -rf would orphan the mounted inode - if the build then failed, the old container
# would keep running against an empty /app/config until someone recreated it. Its
# *contents* are replaced instead, which the mount survives.
REPLACE=(qqbot scripts sql)
FILES=(bot.py requirements.txt Dockerfile docker-compose.yml .dockerignore)

echo "==> sending $(git rev-parse --short HEAD 2>/dev/null || echo 'working tree')"
# Land the whole archive first, and extract it beside the tree rather than over it:
# nothing of the old tree is removed until the new one has extracted whole, so a
# transfer or extract that dies leaves a server that still builds what it ran before.
tar czf - --exclude=__pycache__ "${PAYLOAD[@]}" | "${SSH[@]}" "cat > '$REMOTE/.deploy.tar.gz'"
"${SSH[@]}" "cd '$REMOTE' && rm -rf .deploy.new && mkdir .deploy.new && tar xzf .deploy.tar.gz -C .deploy.new && rm -f .deploy.tar.gz \
  && rm -rf ${REPLACE[*]} && for d in ${REPLACE[*]}; do mv .deploy.new/\$d .; done \
  && mkdir -p config && find config -mindepth 1 -delete && cp -a .deploy.new/config/. config/ \
  && for f in ${FILES[*]}; do mv -f .deploy.new/\$f .; done && rm -rf .deploy.new"

echo "==> rebuilding"
# Keep one step back: tag the running image as :rollback before the build replaces
# it. A deploy that passes the fingerprint check but misbehaves at runtime can then
# be undone from the server alone - see README "Rollback" for the two commands. The
# container is found through compose, so the project name is not assumed.
"${SSH[@]}" "cd '$REMOTE' && cid=\$(docker compose ps -q bot 2>/dev/null); img=\$([ -n \"\$cid\" ] && docker inspect --format '{{.Image}}' \"\$cid\"); [ -n \"\$img\" ] && docker tag \"\$img\" qbot-bot:rollback || true"
"${SSH[@]}" "cd '$REMOTE' && docker compose up -d --build bot"

echo "==> verifying"
# Every path the Dockerfile COPYs code from - a stale bot.py or scripts/ must fail
# the check the same way a stale qqbot/ does.
want=$("$PYTHON" scripts/_fingerprint.py qqbot bot.py scripts)
# tr strips the CR that comes back through ssh from a Windows terminal
got=$("${SSH[@]}" "cd '$REMOTE' && docker compose exec -T bot python /app/scripts/_fingerprint.py /app/qqbot /app/bot.py /app/scripts" | tr -d '\r')

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
