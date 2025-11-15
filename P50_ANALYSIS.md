# Analyse P50 vs Pmin pour HIGH Priority

## Observation
- **Pmin** : ~0.065 ms (65µs) - Latence minimale observée
- **P50** : ~0.157 ms (157µs) - Médiane observée
- **Écart** : ~92µs (P50 est 2.4x plus élevé que Pmin)

## Parcours d'un paquet HIGH

### 1. **Dispatcher** (Premier bottleneck)
```rust
// Ligne 464-501
- try_recv() depuis input queue
- Lock distribution_stats (ligne 483) - CONTENTION possible
- find_best_channel() - calcul de score (3 itérations)
- Clone task (ligne 490-492) - allocation mémoire (~5-10µs)
- send() sur channel (ligne 490-492) - peut bloquer si channel full
- Second lock distribution_stats (ligne 499) - CONTENTION
```
**Overhead estimé** : 10-30µs (si pas de contention), 50-200µs (avec contention)

### 2. **Channel Transit**
```rust
// crossbeam unbounded channel
- Temps de transmission entre dispatcher thread et EDF worker
- Si channel congestionné → délai d'attente
```
**Overhead estimé** : 1-5µs (normal), 10-50µs (si congestion)

### 3. **EDF Worker Receive**
```rust
// Ligne 586-602
- try_recv() depuis channel - peut manquer si worker occupé
- Si HIGH arrive : lock heap (ligne 591)
- heap.push() - O(log n) où n = taille du heap
```
**Overhead estimé** : 5-15µs (heap petit), 20-50µs (heap grand >100)

### 4. **Heap Pop**
```rust
// Ligne 613-616
- Lock heap (ligne 614) - CONTENTION possible si autres workers utilisent
- heap.pop() - O(log n) où n = taille du heap
```
**Overhead estimé** : 5-15µs (heap petit), 20-50µs (heap grand)

### 5. **Processing Time** (Minimal overhead théoriquement)
```rust
// Ligne 673-685
- processing_duration() = 0.05ms (50µs) base + variable selon taille
- Pour petits paquets : 50µs minimum
```
**Overhead fixe** : ~50µs (simulation de traitement)

### 6. **Output Send**
```rust
// Ligne 688
- try_send() vers output queue - peut échouer si queue full
```
**Overhead estimé** : 1-5µs

### 7. **Re-ordering Stage** (Ajoute latence)
```rust
// Ligne 697-760
- Receive depuis EDF output channel
- Stockage dans buffer (head-of-line)
- Calcul earliest deadline (scan 3 buffers)
- Forward vers output queue
```
**Overhead estimé** : 5-20µs

### 8. **CPU Contention** (CRITIQUE)
```rust
// Tous les EDFs (HIGH, MEDIUM, LOW) + Re-ordering sur même core
// Ligne 332: edf_core = worker_cores[0]
```
**Overhead estimé** : 
- **Context switch** : 1-10µs par switch
- **Cache invalidation** : 10-100µs si cache miss
- **Compétition CPU** : HIGH peut attendre que MEDIUM/LOW finissent

## Sources principales de l'écart P50 vs Pmin

### A. **Lock Contention sur distribution_stats** (15-50µs)
- **Problème** : Double lock par HIGH packet (ligne 483 + 499)
- **Impact** : Si plusieurs HIGH arrivent simultanément, ils attendent le lock
- **Solution** : Réduire les locks, utiliser RwLock ou lock-free structure

### B. **CPU Contention** (10-50µs)
- **Problème** : Tous les EDFs sur même core → context switching
- **Impact** : HIGH peut attendre MEDIUM/LOW qui s'exécutent
- **Solution** : Séparer HIGH EDF sur core dédié, ou utiliser thread priorities

### C. **Heap Operations** (5-30µs)
- **Problème** : O(log n) pour push/pop, lock contention possible
- **Impact** : Croît avec la taille du heap
- **Solution** : Limiter taille heap, utiliser lock-free heap ou skip list

### D. **Channel Congestion** (5-20µs)
- **Problème** : Si dispatcher envoie plus vite que workers consomment
- **Impact** : HIGH peut attendre dans channel
- **Solution** : Augmenter capacité channel ou avoir multiple dispatchers

### E. **Re-ordering Stage** (5-20µs)
- **Problème** : Stage supplémentaire qui ajoute latence
- **Impact** : Fixe mais présent pour tous les packets
- **Solution** : Optimiser ou éliminer si possible

### F. **Processing Time Fixe** (50µs)
- **Problème** : Simulation minimum 50µs
- **Impact** : Fixe, ne peut pas être réduit
- **Note** : C'est le "coût" de traitement, pas un overhead

## Calcul théorique

**Pmin (cas idéal)** :
- Dispatcher: 10µs
- Channel: 1µs
- Heap push: 5µs
- Heap pop: 5µs
- Processing: 50µs
- Output: 1µs
- Re-order: 5µs
- **Total: ~77µs** (proche de 65µs observé)

**P50 (cas médian avec contention)** :
- Dispatcher: 20µs (avec lock contention)
- Channel: 3µs
- Heap push: 10µs (heap moyen)
- Heap pop: 10µs
- Processing: 50µs
- Output: 2µs
- Re-order: 10µs
- CPU contention: 30µs (context switches)
- **Total: ~135µs** (proche de 157µs observé)

## Pourquoi P50 n'est pas proche de Pmin ?

**Réponse** : À cause de la **variabilité** introduite par :
1. **Lock contention** (distribution_stats, heaps)
2. **CPU contention** (tous EDFs sur même core)
3. **Heap size variations** (impacte push/pop time)
4. **Channel congestion** sporadique
5. **Context switches** OS (non prévisibles)

Les paquets "chanceux" (Pmin) arrivent quand :
- Pas de contention de lock
- Pas de contexte switch
- Heap petit
- Channels vides
- Core disponible immédiatement

Les paquets "médians" (P50) subissent :
- Contention modérée
- Quelques context switches
- Heap taille moyenne
- Légère congestion de channel

## Recommandations pour réduire P50

### Priorité 1 : Réduire CPU Contention
- **Séparer HIGH EDF sur core dédié** (meilleure solution)
- Ou augmenter thread priority pour HIGH worker

### Priorité 2 : Optimiser Locks
- **Réduire double lock** sur distribution_stats (déjà fait partiellement)
- Utiliser **RwLock** si reads >> writes
- Ou **lock-free structure** pour distribution_stats

### Priorité 3 : Optimiser Heap
- **Limiter taille max heap** (drop oldest if full)
- Ou utiliser **lock-free heap** (crossbeam::SkipList)

### Priorité 4 : Optimiser Channels
- **Augmenter capacity** ou utiliser multiple dispatchers
- Ou **prioritized channels** pour HIGH

### Priorité 5 : Réduire Re-ordering Overhead
- **Optimiser** le scan des 3 buffers
- Ou **éliminer** si pas strictement nécessaire

## Optimisation cible

Avec ces optimisations, on pourrait viser :
- **Pmin** : 50-60µs (juste processing time + overhead minimal)
- **P50** : 70-90µs (Pmin + overhead modéré)
- **Écart** : 20-30µs (au lieu de 90µs)

La clé est de **réduire la variabilité** en éliminant les sources de contention.

