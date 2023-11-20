import wordle

# Instantiate a game object
game = wordle.Wordle(word = 'grape', real_words = True)

# Send your object a guess
print(game.send_guess('adieu'))
