# amux computer-use sandbox (AMUX-5300), built locally on first `amux computer start`.
#
# Base: trycua/cua-ubuntu, CUA's Kasm Ubuntu desktop. We install the HTTP
# /cmd API explicitly on :8000; the base runs noVNC on :6901 and is the one
# CUA desktop image published for linux/arm64 WITH computer-server inside
# (trycua/cua-xfce is amd64-only at 7.4 GB; trycua/xfce-cua ships cua-driver, an
# MCP-over-stdio runtime, without the HTTP API).
FROM trycua/cua-ubuntu:latest

USER root

# Chromium, because the ladder's rung 3 exists for sites a worker could not
# reach through rungs 1-2, and those are Chrome sites. Google Chrome has no
# linux/arm64 build and Ubuntu's chromium is a snap (no snapd in a container),
# so this is Playwright's Chromium build, which ships for both arches.
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/pw-browsers
RUN python3.12 -m playwright install --with-deps chromium \
 && CH="$(ls -d /opt/pw-browsers/chromium-*/chrome-linux*/chrome | head -1)" \
 && test -x "$CH" \
 && ln -sf "$CH" /usr/local/bin/chromium \
 && chmod -R a+rX /opt/pw-browsers \
 && rm -rf /root/.cache /var/lib/apt/lists/*

# Install the existing /cmd protocol at build time with its supported Python.
# No package download at sandbox boot. Bind inside the container; Docker still
# publishes this API only on the host's loopback interface.
#
# fastapi<0.145: computer-server 0.3.46 passes websocket_max_size to FastAPI(),
# which only its catch-all **extra accepted, and 0.145.0 removed that. An image
# rebuilt on 2026-10-22 resolved 0.161.0, the server died on import, Kasm
# printed "Unknown Service: custom_startup" forever, and every `amux computer
# start` timed out at 180s. The build now makes the same FastAPI() call
# (computer_server.main itself needs a display to import), so a break like
# that fails the image build. The server's own output goes to
# /tmp/computer-server.log, which a failed start reports.
RUN python3.12 -m pip install 'cua-computer-server[linux]==0.3.46' 'fastapi<0.145' \
 && python3.12 -c 'import computer_server; from fastapi import FastAPI; FastAPI(websocket_max_size=1)' \
 && printf '#!/bin/bash\nexec python3.12 -m computer_server --host 0.0.0.0 >>/tmp/computer-server.log 2>&1\n' > /dockerstartup/custom_startup.sh \
 && chmod +x /dockerstartup/custom_startup.sh

LABEL amux-computer.image="1"
USER 1000
