#!/bin/bash
# Sets the _SYSTEM password on first boot. The image's own IRIS_PASSWORD
# handling silently fails on this tag, so we apply it directly.
# NOTE: Security.Users.%OpenId("_SYSTEM") returns nothing (its ID is the
# numeric internal id), so resolve the object via Exists(, .user) instead.
iris session IRIS -U%SYS <<-'EOSESS' > /dev/null
set sc = ##class(Security.Users).Exists("_SYSTEM", .user)
if $isobject(user) { set user.PasswordExternal = "SYS", sc = user.%Save() }
halt
EOSESS
echo "irisnative: _SYSTEM password set"
