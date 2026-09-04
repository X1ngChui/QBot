"""The domain model: concepts only. It knows nothing of the database or of any model API.

Dependencies point one way (design doc 62):

    domain  <-  services  <-  repositories  <-  infrastructure

So this layer imports nothing above it. It can be imported and tested on its own, with no
database connection and no NoneBot runtime.
"""
