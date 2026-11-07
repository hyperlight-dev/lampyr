var builder = WebApplication.CreateEmptyBuilder(new WebApplicationOptions
{
    Args = args,
});
// Avoid the guest transport's byte-consuming zero-length readiness receive.
builder.WebHost.UseSockets(options => options.WaitForDataBeforeAllocatingBuffer = false);
builder.WebHost.UseKestrelCore();
builder.Services.AddRouting();
var app = builder.Build();
Application.MapRoutes(app);
var host = Environment.GetEnvironmentVariable("LAMPYR_LISTEN_HOST") ?? "0.0.0.0";
app.Urls.Add($"http://{host}:{Environment.GetEnvironmentVariable("PORT") ?? "8080"}");
await app.RunAsync();
