# pigmenfarm

minecraft pigmen farm bot uzing [azalea-rs](https://github.com/azalea-rs/azalea)

## How to use

1. clone repo
1. Get a looting + mending sword and stand at your pigmen farm., make sure they cant get to you
2. Put the sword in your first hotbar slot, and put food in any other inventory slot. For being extra safe you can bring totems, the bot will put them into offhand
3. Disconnect from the server
4. Run `cargo run --release -- --server <server-ip> --username <username> --login-password [your-password-optional]`. Currently only offline mode is supported

the bot will disconnect if any is true:
- health is <= 3 hearts
- the sword is about to break (<= 20 durability)
