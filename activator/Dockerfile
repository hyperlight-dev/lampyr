FROM golang:1.26-bookworm AS build

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY cmd/ ./cmd/
COPY internal/ ./internal/
RUN CGO_ENABLED=0 go build -o /out/fiber-router ./cmd/fiber-router

FROM ubuntu:24.04
COPY --from=build /out/fiber-router /usr/local/bin/fiber-router
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiber-router"]
