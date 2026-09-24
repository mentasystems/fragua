// Command fragua is the AI-native PCB design tool (Go host).
//
//	fragua               print usage + script reference
//	fragua help [verb]   same, or help for one script verb
//	fragua run [file]    start HTTP API + open browser
//	fragua mcp [file]    same host, plus an MCP server on stdio
//	fragua init [dir]    write agent onboarding files into a directory
//	fragua bench [dir]   run the reference bench suite (place → route → drc)
//	fragua render FILE   3D product-shot PNG (--3d, -o, --width)
package main

import (
	"fmt"
	"os"

	"github.com/mentasystems/fragua/internal/host"
	"github.com/mentasystems/fragua/internal/script"
)

func main() {
	args := os.Args[1:]
	if len(args) == 0 || args[0] == "help" || args[0] == "-h" || args[0] == "--help" {
		if len(args) > 1 {
			out, ok := script.VerbUsage(args[1])
			fmt.Print(out)
			if !ok {
				os.Exit(2)
			}
			return
		}
		fmt.Print(script.Usage())
		return
	}

	var err error
	switch args[0] {
	case "run":
		err = host.Run(arg(args, 1))
	case "mcp":
		err = runMCP(arg(args, 1))
	case "init":
		err = runInit(args[1:])
	case "bench":
		err = runBench(args[1:])
	case "render", "render3d":
		err = runRender(args[1:])
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q — try `fragua help` or `fragua run [file.fragua]`\n", args[0])
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "fragua: %v\n", err)
		os.Exit(1)
	}
}

// arg returns args[i] or "".
func arg(args []string, i int) string {
	if len(args) > i {
		return args[i]
	}
	return ""
}
