"""The domain model: concepts only. It knows nothing of the database or of any model API.

Dependencies point one way. Each layer imports only the ones to its right, and every
one of them imports this package:

    workers / plugins  ->  services  ->  repositories  ->  db / providers
    (all of the above) ->  domain

So this layer imports nothing else in the package. It can be imported and tested on
its own, with no database connection and no NoneBot runtime.
"""
