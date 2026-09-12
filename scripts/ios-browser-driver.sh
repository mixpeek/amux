#!/usr/bin/env bash
# Native iOS browser driver for amux. Appium owns XCTest startup for the selected
# simulator. This helper never starts a worker or touches a physical iOS device.
set -euo pipefail
IOS_DRIVER_HOME="${AMUX_IOS_DRIVER_HOME:-${AMUX_HOME:-$HOME/.amux}/browser-ios-driver}"
IOS_DRIVER_PORT="${AMUX_IOS_WEBDRIVER_PORT:-18102}"
export APPIUM_HOME="$IOS_DRIVER_HOME/home"
case "${1:-run}" in
  setup)
    mkdir -p "$IOS_DRIVER_HOME"
    npm install --prefix "$IOS_DRIVER_HOME" --no-audit --no-fund appium@3.7.0 appium-xcuitest-driver@12.12.3
    if [[ ! -f "$APPIUM_HOME/node_modules/.cache/appium/extensions.yaml" ]]; then
      node "$IOS_DRIVER_HOME/node_modules/appium/build/lib/main.js" driver install --source=local "$IOS_DRIVER_HOME/node_modules/appium-xcuitest-driver"
    fi
    printf 'Native iOS driver installed. Run scripts/ios-browser-driver.sh run; set AMUX_IOS_WEBDRIVER_PORT=%s in amux server.env.\n' "$IOS_DRIVER_PORT"
    ;;
  run)
    if [[ ! -f "$IOS_DRIVER_HOME/node_modules/appium/build/lib/main.js" ]]; then
      printf 'iOS native driver unavailable: run scripts/ios-browser-driver.sh setup first.\n' >&2
      exit 1
    fi
    exec node "$IOS_DRIVER_HOME/node_modules/appium/build/lib/main.js" --address 127.0.0.1 --port "$IOS_DRIVER_PORT" --use-drivers xcuitest --log-level warn --log-timestamp
    ;;
  install-agent)
    [[ "$(uname -s)" == Darwin ]] || { printf 'Simulator requires macOS.\n' >&2; exit 1; }
    [[ -f "$IOS_DRIVER_HOME/node_modules/appium/build/lib/main.js" ]] || { printf 'Run setup first.\n' >&2; exit 1; }
    export IOS_DRIVER_HOME IOS_DRIVER_PORT
    export IOS_NODE_PATH="$(command -v node)"
    python3 - <<'PY'
import os,pathlib,plistlib
home=pathlib.Path.home()
driver=pathlib.Path(os.environ['IOS_DRIVER_HOME'])
port=int(os.environ['IOS_DRIVER_PORT'])
assert 1024 <= port <= 65535
logs=driver/'logs';logs.mkdir(parents=True,exist_ok=True)
plist=home/'Library/LaunchAgents/com.amux.browser-ios-driver.plist'
plist.parent.mkdir(parents=True,exist_ok=True)
data={'Label':'com.amux.browser-ios-driver','RunAtLoad':True,'KeepAlive':True,'ThrottleInterval':10,
 'ProgramArguments':[os.environ['IOS_NODE_PATH'],str(driver/'node_modules/appium/build/lib/main.js'),
 '--address','127.0.0.1','--port',str(port),'--use-drivers','xcuitest','--log-level','warn','--log-timestamp'],
 'EnvironmentVariables':{'APPIUM_HOME':str(driver/'home'),'PATH':os.environ['PATH']},
 'StandardOutPath':str(logs/'driver.log'),'StandardErrorPath':str(logs/'driver.log')}
with plist.open('wb') as f:plistlib.dump(data,f)
plist.chmod(0o600)
PY
    launchctl bootout "gui/$(id -u)/com.amux.browser-ios-driver" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.amux.browser-ios-driver.plist"
    printf 'Installed supervised loopback iOS driver on port %s.\n' "$IOS_DRIVER_PORT"
    ;;
  *) printf 'Usage: scripts/ios-browser-driver.sh setup|run|install-agent\n' >&2; exit 2 ;;
esac
