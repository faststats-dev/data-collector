#!/bin/sh
# The private renderer enforces isolation with seccomp and container limits;
# Chromium user namespaces are unavailable in App Platform.
exec /usr/local/bin/chromium-headless-real --no-sandbox "$@"
