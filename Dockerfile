# Pinned to the Debian release by name: the pg-client-17 install below assumes
# trixie, and a floating -slim tag would follow the next Debian silently.
FROM python:3.12-slim-trixie

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    TZ=Asia/Shanghai

WORKDIR /app

# pg_dump for the daily backup job (section 7.4). The client major version must be >= the
# server's 17, and the trixie base ships exactly 17 - so no third-party repo, no key
# fetching, and one less host the build has to reach.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates postgresql-client-17; \
    rm -rf /var/lib/apt/lists/*; \
    pg_dump --version

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY bot.py ./
COPY qqbot ./qqbot
COPY scripts ./scripts

CMD ["python", "bot.py"]
