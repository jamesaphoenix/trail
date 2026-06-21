# trail (Go wrapper)

A thin Go wrapper that shells out to the `trail` binary and parses its JSON. The
CLI is the source of truth. Requires `trail` on `PATH` (or set `TRAIL_BIN`).

```go
package main

import (
	"fmt"
	trail "github.com/jamesaphoenix/trail/wrappers/go"
)

func main() {
	c := trail.New("/repo")
	c.Init()
	for {
		f, err := c.Claim("refine", "worker-1") // blocks/retries on none-available
		if err != nil {
			panic(err)
		}
		if f == nil { // sweep complete
			break
		}
		// ... investigate f.Path ...
		found := 3
		c.Done("refine", f.Path, "worker-1", &found)
	}
	fmt.Println(c.Status("refine"))
}
```

`Claim` returns `(nil, nil)` when the sweep is complete and retries internally
while folders are only leased elsewhere (exit 4). `Done`/`Skip` take an optional
`*int` findings count for outcome weighting.
