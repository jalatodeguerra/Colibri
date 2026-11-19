class Node:
    __slots__ = ("value", "next")

    def __init__(self, value, next):
        self.value = value
        self.next = next


def build(n):
    head = None
    for i in range(n):
        head = Node(i, head)
    return head


def main():
    total = 0
    for _ in range(20):
        cur = build(50000)
        while cur is not None:
            total += cur.value
            cur = cur.next
    print(total)


main()
