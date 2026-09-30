def marmalade_yield(oranges_kg: float) -> int:
    """Jars of marmalade from a batch of oranges."""
    sugar_kg = oranges_kg * 2
    return round((oranges_kg + sugar_kg) / 0.45)


if __name__ == "__main__":
    print(marmalade_yield(1.0))
