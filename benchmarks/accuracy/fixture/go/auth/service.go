package auth

type GoUser struct{}

func CreateOrder() {}

func Run() {
	CreateOrder()
}

type Storer interface {
	Get(key string) string
}
