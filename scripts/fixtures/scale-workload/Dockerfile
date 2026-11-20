# syntax=docker/dockerfile:1
FROM golang:1.26.7-bookworm@sha256:e8c859f5632dcfde7b32d2012b4351728f6437930887c2f6a91ea242459e5514 AS build
WORKDIR /src
COPY docs/examples/hello-go-standalone/go.mod docs/examples/hello-go-standalone/go.sum ./
RUN go mod download
COPY docs/examples/hello-go-standalone/application.go docs/examples/hello-go-standalone/main.go ./
RUN CGO_ENABLED=0 go build -mod=readonly -trimpath -ldflags="-s -w" -o /service .

FROM scratch
COPY --from=build /service /service
ENV PORT=8080
USER 65532:65532
ENTRYPOINT ["/service"]
