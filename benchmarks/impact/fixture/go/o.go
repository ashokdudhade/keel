package o

import "example.com/impact/n"

func Alert(nb n.Notifier) {
	nb.Notify("hi")
}
