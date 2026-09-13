FROM node:24-bookworm-slim

# Match the driver bundled by playwright-rs 0.18.0. Keep these versions together.
WORKDIR /opt/browser
RUN npm install --omit=dev --save-exact playwright@1.63.0 \
    && npx playwright install --with-deps chrome \
    && rm -rf /var/lib/apt/lists/* /root/.npm
RUN apt-get update && apt-get install -y --no-install-recommends tini \
    && rm -rf /var/lib/apt/lists/*
RUN npx playwright install ffmpeg
COPY server.cjs ./server.cjs
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["node", "server.cjs"]
