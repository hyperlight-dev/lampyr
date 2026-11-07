const http = require('node:http');
let count = 0;
http.createServer((request, response) => {
  response.writeHead(200, {'content-type': 'application/json'});
  response.end(JSON.stringify({kind: 'http', fiberId: process.env.LAMPYR_FIBER_ID, count: ++count}));
}).listen(Number(process.env.PORT), process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
