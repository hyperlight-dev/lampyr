package main

import (
	"log"
	"net"
	"os"

	"github.com/gin-gonic/gin"
)

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	host := os.Getenv("LAMPYR_LISTEN_HOST")
	if host == "" {
		host = "0.0.0.0"
	}
	router := gin.New()
	mapApplication(router)
	log.Fatal(router.Run(net.JoinHostPort(host, port)))
}
