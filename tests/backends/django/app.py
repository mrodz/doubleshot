import sys
import subprocess
import os
import time

def ensure_dependencies():
    """Checks for required third-party packages and installs them if missing."""
    required_packages = ["django", "uvicorn"]
    missing_packages = []
    
    for package in required_packages:
        try:
            __import__(package)
        except ImportError:
            missing_packages.append(package)
            
    if missing_packages:
        print(f"Missing dependencies detected: {', '.join(missing_packages)}")
        print("Installing via pip...")
        try:
            subprocess.check_call([sys.executable, "-m", "pip", "install", *missing_packages])
            print("Dependencies installed successfully.\n" + "-"*40)
        except subprocess.CalledProcessError as e:
            print(f"Failed to install dependencies. Error: {e}")
            sys.exit(1)


ensure_dependencies()


import uvicorn
from django.conf import settings
from django.core.asgi import get_asgi_application
from django.http import HttpResponse
from django.urls import path

# Simulate a slow startup sequence BEFORE initializing the web framework
STARTUP_DELAY = int(os.environ.get("STARTUP_DELAY", "10"))
print(f"Initializing heavy resources... (Sleeping for {STARTUP_DELAY} seconds)")
time.sleep(STARTUP_DELAY)
print("Initialization complete.")

# Extract configuration from environment variables
APP_VERSION = os.environ.get("APP_VERSION", "Blue (Default)")
PORT = int(os.environ.get("PORT", "8000"))

# Inline Django Configuration
if not settings.configured:
    settings.configure(
        DEBUG=True,
        ROOT_URLCONF=__name__,
        SECRET_KEY="not-a-secret-just-for-testing-blue-green",
        ALLOWED_HOSTS=["*"],
    )

# View Handlers
def index(request):
    return HttpResponse(f"Hello from version: {APP_VERSION}\n", content_type="text/plain")

def health(request):
    return HttpResponse("OK\n", content_type="text/plain")

# Routing Mapping
urlpatterns = [
    path("", index),
    path("health", health),
]

# Expose the ASGI application entrypoint
application = get_asgi_application()

if __name__ == "__main__":
    print(f"[{APP_VERSION}] listening on http://0.0.0.0:{PORT}")
    
    # Uvicorn acts as the production-ready ASGI server that listens 
    # for SIGTERM signals out of the box to finish ongoing requests.
    uvicorn.run(
        application,
        host="0.0.0.0",
        port=PORT,
        log_level="info",
    )
