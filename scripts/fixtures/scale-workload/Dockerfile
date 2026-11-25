# syntax=docker/dockerfile:1
FROM node:24-alpine@sha256:116b800c3754b440ee0aa3ed2f20f5bf3d99a554bfd9adeea09ea8017f6e85f5
WORKDIR /app
COPY docs/examples/hello-nodejs-standalone/package.json docs/examples/hello-nodejs-standalone/package-lock.json ./
RUN npm ci --omit=dev
COPY docs/examples/hello-nodejs-standalone/application.js docs/examples/hello-nodejs-standalone/server.js ./
ENV PORT=8080
USER 65532:65532
ENTRYPOINT ["node", "server.js"]
