import json, os, pathlib, subprocess, sys, time
name, *command = sys.argv[1:]
started = time.monotonic()
result = subprocess.run(command, check=False)
record = {"command": command, "elapsed_seconds": round(time.monotonic() - started, 3), "exit_code": result.returncode}
path = pathlib.Path(os.environ["RUNNER_TEMP"]) / "validation" / (name + ".json")
path.write_text(json.dumps(record, indent=2) + "\n")
sys.exit(result.returncode)
