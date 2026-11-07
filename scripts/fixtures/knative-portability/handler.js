const {Response} = require('./.lampyr-adapter/types');

function handle(request) {
  return Response.json({
    message: 'portable Knative Service',
    path: request.path,
    revision: request.headers.get('knative-serving-revision') || '',
    namespace: request.headers.get('knative-serving-namespace') || '',
  });
}

function handleEvent(_event, context) {
  return handle(context.request);
}

module.exports = {handle, handleEvent};
