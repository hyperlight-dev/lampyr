package main

import (
	"net/http"

	"github.com/gin-gonic/gin"
)

func mapApplication(router *gin.Engine) {
	router.GET("/*path", func(context *gin.Context) {
		context.JSON(http.StatusOK, gin.H{
			"message": "hello from Go",
			"path":    context.Request.URL.Path,
		})
	})
}
