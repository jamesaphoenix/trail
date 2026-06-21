// Package trail is a thin Go wrapper around the `trail` coverage-scheduler CLI.
//
// It shells out to the `trail` binary and parses its JSON; the CLI is the source
// of truth. The binary is located via the TRAIL_BIN environment variable, else
// `trail` on PATH.
package trail

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// Exit codes mirrored from the CLI.
const (
	ExitOK            = 0
	ExitError         = 1
	ExitSweepComplete = 3
	ExitNoneAvailable = 4
)

// Client is bound to a project root.
type Client struct {
	Root string
	// PollInterval is how long Claim waits between none-available retries.
	PollInterval time.Duration
}

// New returns a Client for the given project root ("" = current directory).
func New(root string) *Client {
	return &Client{Root: root, PollInterval: 2 * time.Second}
}

// Folder is the payload of a successful next/claim.
type Folder struct {
	Status         string  `json:"status"`
	Task           string  `json:"task"`
	Sweep          int64   `json:"sweep"`
	Path           string  `json:"path"`
	Score          float64 `json:"score"`
	LeaseExpiresAt int64   `json:"lease_expires_at"`
	Remaining      int64   `json:"remaining"`
}

func binName() string {
	if b := os.Getenv("TRAIL_BIN"); b != "" {
		return b
	}
	return "trail"
}

// run executes the binary and returns (exitCode, stdout, stderr).
func (c *Client) run(args ...string) (int, []byte, []byte, error) {
	full := args
	if c.Root != "" {
		full = append([]string{"--root", c.Root}, args...)
	}
	cmd := exec.Command(binName(), full...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	code := 0
	if ee, ok := err.(*exec.ExitError); ok {
		code = ee.ExitCode()
	} else if err != nil {
		return -1, nil, nil, err
	}
	out := bytes.TrimSpace(stdout.Bytes())
	if code == ExitError {
		msg := lastLine(stderr.String())
		if msg == "" {
			msg = "trail error"
		}
		return code, out, stderr.Bytes(), fmt.Errorf("trail: %s", msg)
	}
	return code, out, stderr.Bytes(), nil
}

func lastLine(s string) string {
	s = strings.TrimSpace(s)
	if s == "" {
		return ""
	}
	lines := strings.Split(s, "\n")
	return lines[len(lines)-1]
}

func (c *Client) outcomeArgs(found *int, clean bool) []string {
	if clean {
		return []string{"--clean"}
	}
	if found != nil {
		return []string{"--found", strconv.Itoa(*found)}
	}
	return nil
}

// Init scans the tree and registers the folder snapshot.
func (c *Client) Init() (map[string]any, error) {
	_, out, _, err := c.run("init")
	if err != nil {
		return nil, err
	}
	var m map[string]any
	return m, json.Unmarshal(out, &m)
}

// Claim returns the next folder. It blocks and retries while folders are only
// leased elsewhere, and returns (nil, nil) when the sweep is complete.
func (c *Client) Claim(task, agent string) (*Folder, error) {
	args := []string{"next", "--task", task}
	if agent != "" {
		args = append(args, "--agent", agent)
	}
	for {
		code, out, _, err := c.run(args...)
		if err != nil {
			return nil, err
		}
		switch code {
		case ExitOK:
			var f Folder
			if err := json.Unmarshal(out, &f); err != nil {
				return nil, err
			}
			return &f, nil
		case ExitSweepComplete:
			return nil, nil
		case ExitNoneAvailable:
			time.Sleep(c.PollInterval)
		default:
			return nil, fmt.Errorf("trail: unexpected exit code %d", code)
		}
	}
}

func (c *Client) complete(verb, task, path, agent string, found *int, clean bool, reason string) (map[string]any, error) {
	args := []string{verb, "--task", task, "--path", path}
	if agent != "" {
		args = append(args, "--agent", agent)
	}
	if reason != "" {
		args = append(args, "--reason", reason)
	}
	args = append(args, c.outcomeArgs(found, clean)...)
	_, out, _, err := c.run(args...)
	if err != nil {
		return nil, err
	}
	var m map[string]any
	return m, json.Unmarshal(out, &m)
}

// Done marks a folder covered. Pass found (or nil) to report findings.
func (c *Client) Done(task, path, agent string, found *int) (map[string]any, error) {
	return c.complete("done", task, path, agent, found, false, "")
}

// Skip marks a folder covered-but-skipped.
func (c *Client) Skip(task, path, agent, reason string, found *int) (map[string]any, error) {
	return c.complete("skip", task, path, agent, found, false, reason)
}

// Status returns the coverage snapshot for the latest sweep.
func (c *Client) Status(task string) (map[string]any, error) {
	_, out, _, err := c.run("status", "--task", task)
	if err != nil {
		return nil, err
	}
	var m map[string]any
	return m, json.Unmarshal(out, &m)
}
