def main():
    xs = []
    for i in range(1000000):
        xs.append(i)
    total = 0
    for i in range(1000000):
        total += xs[i]
    for x in xs:
        total += x
    print(total)


main()
