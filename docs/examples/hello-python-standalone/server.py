import os
from pathlib import Path
import sys

import uvicorn

# The guest uses runpy, which does not add the script directory to sys.path.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from application import app

if __name__ == "__main__":
    uvicorn.run(app, host=os.environ.get("LAMPYR_LISTEN_HOST", "0.0.0.0"), port=int(os.environ.get("PORT", "8080")),
                loop="asyncio", http="h11")
