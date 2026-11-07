const fs = require('node:fs');
const {Body, Headers, Response, HandlerError} = require('./.lampyr-adapter/types');
const closedBodies = [];

async function handle(request, context) {
  switch (request.path) {
    case '/closed-bodies': return Response.json(closedBodies);
    case '/close-head':
    case '/close-204':
    case '/close-304': {
      const source = {
        [Symbol.iterator]() { return this; },
        next() { throw new Error('bodyless response source was read'); },
        return() { closedBodies.push(request.path); return {done: true}; },
      };
      return new Response(new Body(source), {
        status: request.path === '/close-204' ? 204 : request.path === '/close-304' ? 304 : 200,
      });
    }
    case '/value': return Response.json({ok: true}, {status: 201, headers: new Headers([['x-contract', 'value']])});
    case '/bytes': return Response.bytes(Buffer.from('raw\0bytes'));
    case '/text': return Response.text('plain text', {headers: new Headers([['Content-Type', 'text/plain']])});
    case '/array': return Response.json([1, 2, 3]);
    case '/stream': return new Response(new Body([Buffer.from('one\n'), Buffer.from('two\n'), Buffer.from('three\n')]));
    case '/typed-error': throw new HandlerError(422, '<bad>\r\nX-Injected: yes');
    case '/error': throw new Error('private details');
    case '/serialize-error': {
      const body = {};
      body.self = body;
      return Response.json(body);
    }
    case '/empty': return Response.text('must not appear', {status: 204});
    case '/headers': return Response.text('bad', {headers: new Headers([['content-length', '999']])});
    case '/unicode-header': return Response.text('bad', {headers: new Headers([['x-locale', 'snowman ☃']])});
    case '/not-modified': return Response.json({ok: true}, {status: 304});
    case '/broken-stream': return new Response(new Body((function* () { yield Buffer.from('first'); throw new Error('stream failed'); })()));
    case '/upload-stream': return new Response(request.body);
    case '/json-body': return Response.json(await request.body.json());
    case '/consume-twice':
      await request.body.bytes();
      return Response.bytes(await request.body.bytes());
    case '/read-limit': return Response.bytes(await request.body.bytes(4));
    case '/http-context': return Response.json({requestInContext: Object.hasOwn(context, 'request')});
    case '/own-nosniff': return Response.text('own', {headers: new Headers([['x-content-type-options', 'handler']])});
    case '/repeated-headers': return Response.json(
      {values: request.headers.getAll('x-repeat')},
      {headers: new Headers([['set-cookie', 'a=1'], ['set-cookie', 'b=2']])});
    case '/backpressure': return new Response(new Body((function* () {
      const chunk = Buffer.alloc(1024 * 1024, 120);
      for (let index = 1; index <= 128; index++) {
        fs.writeFileSync('produced', String(index));
        yield chunk;
      }
    })()));
    case '/cancel': return new Response(new Body((async function* () {
      try {
        while (!context.signal.aborted) {
          yield Buffer.alloc(65536, 120);
          await new Promise(resolve => setTimeout(resolve, 5));
        }
      } finally { fs.writeFileSync('cancelled', 'closed'); }
    })()));
    default: return Response.json({method: request.method, path: request.path,
      body: await request.body.text(), header: request.headers.get('x-test')});
  }
}
module.exports = {handle};
