def main():
    parts = [f"item {i}" for i in range(200000)]
    back = ",".join(parts).split(",")
    print(sum(1 for s in back if s.startswith("item 1")))


main()
