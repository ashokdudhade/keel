package e

type Widget struct {
	Value int
}

func Unwrap(v any) *Widget {
	w, _ := v.(*Widget)
	return w
}

func Build() Widget {
	return Widget{}
}
