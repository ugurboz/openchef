// Command oracle prints the Go implementation's Split result for every command line
// read from stdin (a JSON array of strings), as a JSON array, for the S4 comparison.
// shell.go is a verbatim copy of internal/permissions/shell.go at tag go-reference.
package main

import (
	"encoding/json"
	"fmt"
	"os"
)

type result struct {
	Input    string   `json:"input"`
	Error    string   `json:"error,omitempty"`
	Commands []Simple `json:"commands"`
}

func main() {
	var inputs []string
	if err := json.NewDecoder(os.Stdin).Decode(&inputs); err != nil {
		fmt.Fprintln(os.Stderr, "decode:", err)
		os.Exit(1)
	}
	out := make([]result, 0, len(inputs))
	for _, in := range inputs {
		cmds, err := Split(in)
		r := result{Input: in, Commands: cmds}
		if err != nil {
			r.Error = "unparsable"
			r.Commands = nil
		}
		if r.Commands == nil {
			r.Commands = []Simple{}
		}
		out = append(out, r)
	}
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	if err := enc.Encode(out); err != nil {
		fmt.Fprintln(os.Stderr, "encode:", err)
		os.Exit(1)
	}
}
