In this library, I've continued my personal diving into the world of synchronization 

Library will consist of lock-free building blocks:
- Hazard Pointer (regular) ✅
- HP (Pass-the-buck version) 🚧
- RCU (single writer) ✅

And basic structures:
- Treiber Stack w/elimination backoff ✅
- Michael-Scott Queue (regular (almost)) ✅
- Michael-Scott Queue (optimistic version) ✅
- Lock-free HashMap ⛔️
- Lock-free SkipList ⛔️
- Parking Lot ⛔️
- Sequence Lock ⛔️
