def main():
    s = 0
    for i in range(5000000):
        s += i % 7
    print(s)


main()
