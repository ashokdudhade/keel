package d

import "example.com/impact/a"

type Worker struct{}

func (w Worker) Run() int {
    return a.Alpha()
}
