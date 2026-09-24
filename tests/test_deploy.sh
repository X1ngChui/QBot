#!/usr/bin/env bash
# Exercise deployment ordering against a local fake host, never over SSH.

set -euo pipefail
cd "$(dirname "$0")/.."
sandbox=$(mktemp -d)
trap 'rm -rf "$sandbox"' EXIT
remote="$sandbox/remote"
mkdir -p "$sandbox/bin" "$remote/config"
trace="$sandbox/trace"

cat > "$sandbox/bin/ssh" <<'EOF'
#!/usr/bin/env bash
command=${!#}
bash -c "$command"
EOF
cat > "$sandbox/bin/docker" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TRACE"
case "$*" in
    'compose ps -q bot') printf 'fake-bot\n' ;;
    'inspect --format '* ) printf 'fake-image\n' ;;
    'tag '* | 'compose logs '* ) ;;
    'compose build bot') [ "$FAIL_AT" != build ] ;;
    'compose run '* )
        [[ "$*" == *'CONFIG_DIR=/app/config.next'* ]]
        [[ "$*" == *'.deploy.new/config:/app/config.next:ro'* ]]
        [ "$(cat "$QBOT_REMOTE/config/marker")" = old ]
        [ -f "$QBOT_REMOTE/.deploy.new/config/prompts/prompts.yaml" ]
        [ "$FAIL_AT" != schema ]
        ;;
    'compose stop bot') ;;
    'compose up -d --no-build bot')
        [ ! -e "$QBOT_REMOTE/config/marker" ]
        [ -f "$QBOT_REMOTE/config/prompts/prompts.yaml" ]
        [ "$FAIL_AT" != startup ]
        ;;
    'compose exec -T bot python /app/scripts/_fingerprint.py '* )
        if [ "$FAIL_AT" = fingerprint ]; then
            printf 'changed after switch\n' >> "$QBOT_REMOTE/config/prompts/prompts.yaml"
        fi
        python "$QBOT_REMOTE/scripts/_fingerprint.py" "$QBOT_REMOTE/qqbot" \
            "$QBOT_REMOTE/bot.py" "$QBOT_REMOTE/scripts" "$QBOT_REMOTE/config"
        ;;
    *) printf 'unexpected docker call: %s\n' "$*" >&2; exit 1 ;;
esac
EOF
chmod +x "$sandbox/bin/ssh" "$sandbox/bin/docker"
export PATH="$sandbox/bin:$PATH" TRACE="$trace" QBOT_REMOTE="$remote"
export QBOT_HOST=fake PYTHON=python

for phase in build schema startup success fingerprint; do
    rm -rf "$remote"
    mkdir -p "$remote/config"
    printf 'old\n' > "$remote/config/marker"
    : > "$trace"
    if [ "$phase" = success ] || [ "$phase" = fingerprint ]; then
        if [ "$phase" = fingerprint ]; then
            if FAIL_AT=fingerprint bash scripts/deploy.sh > "$sandbox/output" 2>&1; then
                echo '[FAIL] configuration drift unexpectedly passed verification' >&2
                exit 1
            fi
            grep -q '^DEPLOY FAILED: the running container' "$sandbox/output"
            echo '[ok ] configuration drift fails the running fingerprint check'
        else
            FAIL_AT=none bash scripts/deploy.sh > "$sandbox/output" 2>&1
            echo '[ok ] successful deployment switches and verifies the new configuration'
        fi
        [ ! -e "$remote/config/marker" ]
        [ -f "$remote/config/prompts/prompts.yaml" ]
        [ ! -d "$remote/.deploy.new" ]
        grep -q '^compose exec -T bot python /app/scripts/_fingerprint.py ' "$trace"
    else
        if FAIL_AT="$phase" bash scripts/deploy.sh > "$sandbox/output" 2>&1; then
            echo "[FAIL] $phase unexpectedly succeeded" >&2
            exit 1
        fi
        [ "$(cat "$remote/config/marker")" = old ]
        if [ "$phase" = startup ]; then
            [ "$(grep -c '^compose stop bot$' "$trace")" -eq 2 ]
            echo '[ok ] failed startup stops the bot and restores old configuration'
        else
            ! grep -q '^compose stop bot$' "$trace"
            echo "[ok ] failed $phase leaves the old bot and configuration untouched"
        fi
    fi
    if [ "$phase" != build ]; then
        grep -q '^compose run .*CONFIG_DIR=/app/config.next .*\.deploy.new/config:/app/config.next:ro' "$trace"
    fi
done
