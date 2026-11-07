internal static class Application
{
    public static void MapRoutes(WebApplication app)
    {
        app.Map("/{**path}", (HttpContext context) => Results.Json(new
        {
            message = "hello from .NET",
            path = context.Request.Path.Value ?? "/",
        }));
    }
}
