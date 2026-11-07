const serviceName = 'hello-nodejs';
const {Body, Headers, Response} = require('./.lampyr-adapter/types');

/** @param {import('./.lampyr-adapter/types').Request} request
 * @param {import('./.lampyr-adapter/types').Context} context
 * @returns {import('./.lampyr-adapter/types').Response} */
function handle(request, context) {
  console.log(JSON.stringify({
    timestamp: new Date().toISOString(), file: 'handler.js', kind: 'handler',
    service: serviceName, message: 'request received', path: request.path,
  }));
  if (request.path === '/stream') {
    return new Response(new Body((function* () {
        for (let chunk = 1; chunk <= 3 && !context.signal.aborted; chunk++) {
          yield Buffer.from(JSON.stringify({chunk}) + '\n');
        }
      })()), {headers: new Headers([['content-type', 'application/x-ndjson']])});
  }
  return Response.json({message: 'hello from Hyperlight edge', path: request.path});
}

/** @param {import('./.lampyr-adapter/types').Event} event
 * @param {import('./.lampyr-adapter/types').ProtocolContext} context */
function handleEvent(event, context) {
  const path = context.request.path;
  console.log(JSON.stringify({
    timestamp: new Date().toISOString(), file: 'handler.js', kind: 'handler',
    service: serviceName, message: 'event received', path, event,
  }));
  return Response.json({message: 'hello from Hyperlight edge', path, event}, {
    status: path === '/events/fail' ? 500 : path === '/events/knative' ? 204 : 200,
  });
}

/** @param {import('./.lampyr-adapter/types').WebSocketSession} session
 * @param {import('./.lampyr-adapter/types').ProtocolContext} context */
async function handleWebSocket(session, context) {
  if (context.request.path !== '/ws') {
    await session.close(1008, 'not found');
    return;
  }
  for await (const message of session) {
    await session.send(message);
  }
}

/** @type {import('./.lampyr-adapter/types').Hooks} */
module.exports = {handle, handleEvent, handleWebSocket};
