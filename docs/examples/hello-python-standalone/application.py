from fastapi import FastAPI, Request

app = FastAPI(openapi_url=None, docs_url=None, redoc_url=None)


@app.get("/{path:path}")
async def hello(request: Request):
    return {"message": "hello from Python", "path": request.url.path}
