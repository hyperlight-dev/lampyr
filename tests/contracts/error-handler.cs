using Lampyr;

public sealed class Handler : IHandler, IEventHandler
{
    public ValueTask<Response> HandleAsync(Request request, Context context) =>
        throw new HandlerError(int.Parse(request.Path[1..]), "typed error details");

    public ValueTask<Response> HandleEventAsync(CloudEvent cloudEvent, ProtocolContext context) =>
        throw new HandlerError(int.Parse(context.Request.Path[1..]), "typed error details");
}
