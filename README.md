In this library, I'm continuing my personal diving into the world of synchronization 

[todo] (almost everything so far :) )

Library will consist of lock-free building blocks:
- Hazard Pointer (regular) ✅
- HP (Pass-the-buck version) 🚧
- RCU (single writer) ✅

And basic structures:
- Treiber Stack w/elimination backoff 🚧
- Michael-Scott Queue (regular) 🚧
- Michail-Scott Queue (optimistic version) 🚧
- Lock-free HashMap ⛔️
- Lock-free SkipList ⛔️
- Parking Lot ⛔️
- Sequence Lock ⛔️