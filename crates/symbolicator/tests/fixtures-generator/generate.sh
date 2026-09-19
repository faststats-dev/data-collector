#!/bin/sh
# All language tooling stays in disposable containers. Only Docker is required.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
case "${1:-all}" in
  javascript|jvm|all) target=${1:-all} ;;
  *) echo 'usage: generate.sh [javascript|jvm|all]' >&2; exit 2 ;;
esac
mkdir -p "$root/fixtures/generated"
if [ "$target" = javascript ] || [ "$target" = all ]; then
  docker run --rm --platform linux/amd64 \
    -v "$root/fixtures-generator/javascript:/generator:ro" \
    -v "$root/fixtures/generated:/output" \
    node:22.14.0-bookworm@sha256:e5ddf893cc6aeab0e5126e4edae35aa43893e2836d1d246140167ccc2616f5d7 \
    sh -c 'mkdir -p /tmp/generator; cp /generator/package*.json /generator/generate.mjs /tmp/generator/; cp -R /generator/src /tmp/generator/src; cd /tmp/generator; npm ci --ignore-scripts --no-audit --no-fund; node generate.mjs'
fi
if [ "$target" = jvm ] || [ "$target" = all ]; then
  docker run --rm \
    -v "$root/fixtures-generator/jvm:/generator:ro" \
    -v "$root/fixtures/generated:/output" \
    maven:3.9.9-eclipse-temurin-17@sha256:f58d59b6273e785ac0a4477f6e9b5ba1d7731c75b906c0f7b34076f1851318cc sh /generator/generate.sh
fi
