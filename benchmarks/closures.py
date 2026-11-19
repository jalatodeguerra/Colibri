def main():
    xs = list(range(200000))
    total = 0
    for _ in range(10):
        ys = list(filter(lambda x: x % 3 == 0, map(lambda x: x * 2, xs)))
        total += len(ys)
    print(total)


main()
