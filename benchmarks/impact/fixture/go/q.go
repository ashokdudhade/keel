package q

import "example.com/impact/p"

func Make() p.Server {
	return p.Server{Host: "x"}
}
