def main():
    m = {}
    for i in range(300000):
        m[i] = i * 2
    total = 0
    for i in range(300000):
        total += m[i]
    sm = {}
    for i in range(100000):
        sm[f"key{i}"] = i
    for i in range(100000):
        total += sm[f"key{i}"]
    print(total)


main()
