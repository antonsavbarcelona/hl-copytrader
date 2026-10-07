#!/bin/sh
# Container entry point: the paper run, kept in Postgres (or files when there is none).
#   DATABASE_URL  Postgres to keep the run in (tables created on first start); else files:
#   DATA_DIR      where state.json / events.jsonl live (default: Railway's volume, else /data)
#   RUN_ID        name of this run in the database (default main)
#   API_WEIGHT    info API weight per minute (default 800: alone on its IP, of 1200)
#   EXTRA_ARGS    more `run` options, e.g. "--start 2000"
set -e
DATA="${DATA_DIR:-${RAILWAY_VOLUME_MOUNT_PATH:-/data}}"
if [ -z "$DATABASE_URL" ] && [ -n "$RAILWAY_ENVIRONMENT" ] && [ -z "$RAILWAY_VOLUME_MOUNT_PATH" ] && [ -z "$DATA_DIR" ]; then
    echo "WARNING: no DATABASE_URL and no volume: the run starts over on every redeploy"
fi
mkdir -p "$DATA"
# exec: SIGTERM on redeploy / stop reaches the bot, which saves its state.
exec hl-copytrader run --data "$DATA" --weight "${API_WEIGHT:-800}" $EXTRA_ARGS
