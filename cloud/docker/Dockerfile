FROM python:3.11-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux curl git procps \
    && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir requests

WORKDIR /app
COPY amux-server.py .

ENV AMUX_PORT=8822
EXPOSE 8822

CMD ["python", "amux-server.py", "8822", "--no-tls"]
