# Eventix 2 ACSM

This is an application server that acts as a gateway between Eventix and Assetto
Corsa Server Manager.

You can create an event in Eventix to match a Championship in ACSM. And you
create a ticket type per car. Unfortunately you can currently only have one car
in one class. Also configure one slot per ticket for that car.

Also create an OAuth2 Client in Eventix. You will need to figure out the
redirect URL in `.env` first.

Copy `.env-template` to `.env` and change all the settings to match your
configuration in both ACSM and Eventix.
