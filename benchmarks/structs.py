class P:
    __slots__ = ("x", "y", "vx", "vy")

    def __init__(self):
        self.x = 0.0
        self.y = 0.0
        self.vx = 1.0
        self.vy = 2.0

    def step(self, dt):
        self.x += self.vx * dt
        self.y += self.vy * dt


def main():
    ps = [P() for _ in range(1000)]
    for _ in range(300):
        for p in ps:
            p.step(0.016)
    print(ps[0].x)


main()
