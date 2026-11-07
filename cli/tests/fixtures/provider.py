import json
import hyperlight


def invoke(request):
    operation = request["operation"]
    if operation == "describe":
        with open("/provider/manifest.json", encoding="utf-8") as source:
            result = json.load(source)
    elif operation == "init":
        hyperlight.call("workspace.scaffold", {
            "main.bicep": "output original bool = true\n",
        })
        hyperlight.call("state.write", {
            "key": "operation",
            "value": {"installation": request["installationId"]},
        })
        result = {"initialized": True}
    elif operation == "apply":
        with open("/workspace/main.bicep", encoding="utf-8") as source:
            result = {"source": source.read()}
        if request["config"].get("fail"):
            return {
                "protocolVersion": 1, "status": "error",
                "code": "deployment-failed", "message": "deliberate failure",
            }
    elif operation == "inspect":
        result = hyperlight.call("state.read", "operation")
    else:
        raise ValueError("unsupported operation")
    return {"protocolVersion": 1, "status": "ok", "result": result}
