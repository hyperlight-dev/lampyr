const express = require('express');

const app = express();
app.get('/{*path}', (request, response) => {
    response.json({message: 'hello from Node.js', path: request.path});
});

module.exports = app;
