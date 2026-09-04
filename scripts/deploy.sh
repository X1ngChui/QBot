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

# Directories are replaced, not merged. `tar x` only overwrites, so without the rm a
# deleted module would stay on the server forever and get COPYed into every next image -
# and a deleted module that something can still import is a very quiet bug.
# config/ is NOT in this list: it is bind-mounted into the running container, and
# rm -rf would orphan the mounted inode - if the build then failed, the old container
# would keep running against an empty /app/config until someone recreated it. Its
# *contents* are deleted instead, which the mount survives.
REPLACE=(qqbot scripts sql)

echo "==> sending $(git rev-parse --short HEAD 2>/dev/null || echo 'working tree')"
# Land the whole archive first: if the transfer dies, the server still has a working tree.
tar czf - --exclude=__pycache__ "${PAYLOAD[@]}" | "${SSH[@]}" "cat > '$REMOTE/.deploy.tar.gz'"
"${SSH[@]}" "cd '$REMOTE' && rm -rf ${REPLACE[*]} && mkdir -p config && find config -mindepth 1 -delete && tar xzf .deploy.tar.gz && rm -f .deploy.tar.gz"

echo "==> rebuilding"
# Keep one step back: tag the running image as :rollback before the build replaces
# it. A deploy that passes the fingerprint check but misbehaves at runtime can then
# be undone from the server alone - see README "回滚" for the two commands.
"${SSH[@]}" "img=\$(docker inspect --format '{{.Image}}' qbot-bot-1 2>/dev/null); [ -n \"\$img\" ] && docker tag \"\$img\" qbot-bot:rollback || true"
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
