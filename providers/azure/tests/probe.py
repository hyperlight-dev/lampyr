import subprocess


def invoke(request):
    result = subprocess.run(
        request["args"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    return {
        "exit": result.returncode,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }
