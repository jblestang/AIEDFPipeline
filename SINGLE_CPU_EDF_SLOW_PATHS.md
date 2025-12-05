# Analyse des Slow Paths dans SingleCPU EDF

## Vue d'ensemble

Cette analyse identifie les goulots d'étranglement de performance dans le scheduler SingleCPU EDF et propose des optimisations.

## Slow Paths Identifiés

### 1. 🔴 CRITIQUE : Double Lock sur EDF Queue (process_next → refill_edf)

**Emplacement** : `process_next()` ligne 250-257, puis `refill_edf()` ligne 168

**Problème** :
```rust
// STEP 1: Premier lock pour vérifier si vide
for priority in priorities.iter() {
    let edf_queue = &self.edf_queues[*priority];
    let is_empty = edf_queue.lock().is_empty();  // LOCK #1
    
    if is_empty {
        self.refill_edf(*priority);  // Va créer un LOCK #2 sur socket_configs
    }
}

// Dans refill_edf():
fn refill_edf(&self, priority: Priority) -> usize {
    let socket_configs = self.socket_configs.lock();  // LOCK #2
    // ...
    let edf_queue = self.edf_queues[priority].clone();  // CLONE Arc (lent)
    // ...
    edf_queue.lock().push_back(task);  // LOCK #3 sur la même queue
}
```

**Impact** :
- Double acquisition de lock sur `edf_queue` (une pour `is_empty()`, une pour `push_back()`)
- Clone inutile de `Arc<Mutex<VecDeque>>` (clone de l'Arc, pas le contenu)
- Coût : ~50-200ns par lock + ~10ns pour le clone Arc

**Optimisation** :
```rust
// Option 1: Verrouiller une seule fois et vérifier + refill dans le même lock
for priority in priorities.iter() {
    let edf_queue = &self.edf_queues[*priority];
    let mut queue_guard = edf_queue.lock();
    if queue_guard.is_empty() {
        drop(queue_guard);  // Libérer avant refill
        self.refill_edf(*priority);
    }
}

// Option 2: Éviter le clone dans refill_edf
fn refill_edf(&self, priority: Priority) -> usize {
    // ... read from sockets ...
    // Utiliser &self.edf_queues[priority] directement au lieu de clone()
    self.edf_queues[priority].lock().push_back(task);
}
```

**Gain estimé** : 50-150ns par itération

---

### 2. 🟡 MOYEN : Triple Lock pour Comparaison de Deadlines

**Emplacement** : `process_next()` ligne 266-284

**Problème** :
```rust
// STEP 2: Compare deadlines - lock chaque queue pour front()
for priority in priorities.iter() {
    let edf_queue = &self.edf_queues[*priority];
    let queue_guard = edf_queue.lock();  // LOCK séparé pour chaque priorité
    
    if let Some(task) = queue_guard.front() {
        // Comparaison de deadlines
    }
}
```

**Impact** :
- 3 locks séquentiels (HIGH, MEDIUM, LOW) même si seulement HIGH a des paquets
- Coût : ~150-300ns (3 locks × 50-100ns)
- Risque de contention si d'autres threads accèdent (bien qu'ici c'est single-thread)

**Optimisation** :
```rust
// Lock une seule fois et lire toutes les deadlines
let mut deadlines = Vec::with_capacity(3);
for priority in priorities.iter() {
    let edf_queue = &self.edf_queues[*priority];
    let queue_guard = edf_queue.lock();
    if let Some(task) = queue_guard.front() {
        deadlines.push((*priority, task.deadline));
    }
}
// Comparer ensuite sans lock
```

**Gain estimé** : 100-200ns (réduction de 3 locks à 3 locks mais plus courts)

**Note** : Cette optimisation est limitée car on doit quand même verrouiller chaque queue. Une meilleure approche serait d'utiliser un lock-free structure, mais c'est plus complexe.

---

### 3. 🟡 MOYEN : Syscall ioctl dans socket_bytes_available()

**Emplacement** : `refill_edf()` ligne 188 → `datagram_size_hint()` → `socket_bytes_available()`

**Problème** :
```rust
let size_hint = datagram_size_hint(socket);  // Appelle ioctl FIONREAD
// ...
fn socket_bytes_available(socket: &StdUdpSocket) -> std::io::Result<usize> {
    let fd = socket.as_raw_fd();
    let mut bytes: libc::c_int = 0;
    let ret = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) };  // SYSCALL
    // ...
}
```

**Impact** :
- Syscall ioctl : ~200-500ns (transition kernel space)
- Appelé pour chaque socket à chaque refill
- Si 2 sockets par priorité × 3 priorités = 6 syscalls potentiels

**Optimisation** :
```rust
// Option 1: Skip ioctl si socket non-blocking (juste essayer recv_from)
// recv_from sur socket non-blocking retournera WouldBlock si vide
// Économie : skip ioctl, aller directement à recv_from

// Option 2: Batch ioctl pour tous les sockets d'une priorité
// (Mais recv_from sera quand même nécessaire, donc gain limité)

// Option 3: Retirer ioctl complètement, utiliser MAX_PACKET_SIZE
// Coût : allocation légèrement plus grande, mais économise syscall
```

**Gain estimé** : 200-500ns × nombre de sockets (potentiellement 1-6 syscalls évités)

---

### 4. 🟢 FAIBLE : Clone Arc dans refill_edf()

**Emplacement** : `refill_edf()` ligne 176

**Problème** :
```rust
let edf_queue = self.edf_queues[priority].clone();  // Clone de Arc (pas le contenu)
// ...
edf_queue.lock().push_back(task);
```

**Impact** :
- Clone de `Arc` : ~10-20ns (juste incrémenter le compteur de référence)
- Pas vraiment un slow path, mais inutile

**Optimisation** :
```rust
// Utiliser directement &self.edf_queues[priority]
self.edf_queues[priority].lock().push_back(task);
```

**Gain estimé** : 10-20ns (négligeable mais bon à avoir)

---

### 5. 🟡 MOYEN : Lock socket_configs pendant toute la boucle

**Emplacement** : `refill_edf()` ligne 168

**Problème** :
```rust
fn refill_edf(&self, priority: Priority) -> usize {
    let socket_configs = self.socket_configs.lock();  // LOCK au début
    let sockets = &socket_configs[priority];
    
    // ... boucle sur sockets, I/O, etc. ...
    // Lock maintenu pendant toute la fonction (I/O potentiellement lent)
}
```

**Impact** :
- Lock maintenu pendant les opérations I/O (recv_from)
- Si plusieurs threads accèdent (actuellement single-thread, donc pas critique)
- Mais bloque `add_socket()` si appelé en parallèle

**Optimisation** :
```rust
// Cloner la liste des sockets avant I/O
let sockets = {
    let configs = self.socket_configs.lock();
    configs[priority].clone()  // Clone Vec<SocketConfig>
};
// Puis utiliser sockets sans lock
for socket_config in sockets.iter() {
    // I/O sans lock
}
```

**Gain estimé** : Réduction du temps de lock (meilleure pour la contention future)

---

### 6. 🔴 CRITIQUE : Lock Pattern Inefficace dans Comparaison

**Emplacement** : `process_next()` ligne 266-284 + 295-299

**Problème** :
```rust
// STEP 2: Lock pour lire deadline
for priority in priorities.iter() {
    let queue_guard = edf_queue.lock();  // Lock #1 pour HIGH
    // ...
    if let Some(task) = queue_guard.front() { ... }
}  // Lock relâché

// STEP 3: Re-lock la même queue pour pop_front
let task = {
    let mut queue_guard = edf_queue.lock();  // Lock #2 pour la même queue
    queue_guard.pop_front()
};
```

**Impact** :
- Double lock sur la queue sélectionnée (une pour `front()`, une pour `pop_front()`)
- Coût : ~100-200ns (2 locks)

**Optimisation** :
```rust
// Option 1: Garder le lock de la comparaison et pop immédiatement
let mut selected_queue = None;
let mut selected_deadline = None;

for priority in priorities.iter() {
    let edf_queue = &self.edf_queues[*priority];
    let mut queue_guard = edf_queue.lock();  // Lock une fois
    
    if let Some(task) = queue_guard.front() {
        match selected_deadline {
            None => {
                selected_deadline = Some(task.deadline);
                selected_queue = Some((priority, queue_guard));
            }
            Some(deadline) if task.deadline < deadline => {
                // Nouveau candidat plus tôt
                if let Some((_, old_guard)) = selected_queue.take() {
                    drop(old_guard);  // Libérer l'ancien lock
                }
                selected_deadline = Some(task.deadline);
                selected_queue = Some((priority, queue_guard));
            }
            _ => {}
        }
    } else {
        drop(queue_guard);  // Libérer si pas sélectionné
    }
}

// Utiliser le queue_guard gardé pour pop
if let Some((priority, mut queue_guard)) = selected_queue {
    let task = queue_guard.pop_front().unwrap();
    // ...
}
```

**Note** : Cette optimisation est complexe à cause de la gestion des locks multiples. Une approche plus simple :

```rust
// Option 2: Lock une seule fois avec peek + pop combiné
// Nécessite une méthode peek_and_pop() ou similaire
```

**Gain estimé** : 50-100ns par packet (réduction de 2 locks à 1 lock)

---

### 7. 🟢 FAIBLE : thread::yield_now() quand pas de paquets

**Emplacement** : `run()` ligne 368

**Problème** :
```rust
if !self.process_next() {
    std::thread::yield_now();  // Yield au scheduler OS
}
```

**Impact** :
- `yield_now()` : ~1-10µs (transition au scheduler OS)
- Acceptable si vraiment pas de paquets, mais pourrait être optimisé

**Optimisation** :
```rust
// Option 1: Petit spin loop avant yield
let mut spins = 0;
while !self.process_next() && spins < 10 {
    std::hint::spin_loop();
    spins += 1;
}
if spins >= 10 {
    std::thread::yield_now();
}

// Option 2: Polling plus agressif avec timeout
```

**Gain estimé** : Latence réduite quand paquets arrivent (1-10µs → 100-500ns)

---

## Recommandations par Priorité

### 🔴 Priorité HAUTE (Impact significatif)

1. **Éliminer le double lock dans process_next → refill_edf**
   - Gain : 50-150ns
   - Complexité : Faible
   - Risque : Faible

2. **Réduire les locks dans la comparaison de deadlines**
   - Gain : 50-100ns
   - Complexité : Moyenne
   - Risque : Faible

### 🟡 Priorité MOYENNE (Impact modéré)

3. **Éviter ioctl syscall dans socket_bytes_available**
   - Gain : 200-500ns × nombre de sockets
   - Complexité : Faible
   - Risque : Très faible

4. **Optimiser le lock socket_configs**
   - Gain : Meilleure scalabilité future
   - Complexité : Faible
   - Risque : Très faible

### 🟢 Priorité BASSE (Impact faible)

5. **Éliminer clone Arc inutile**
   - Gain : 10-20ns
   - Complexité : Très faible
   - Risque : Aucun

6. **Optimiser yield_now**
   - Gain : Latence réduite
   - Complexité : Faible
   - Risque : Faible

## Métriques de Performance Attendues

### Avant Optimisations
- Lock acquisitions par packet : ~6-8 locks
- Syscalls par refill : 0-6 ioctl calls
- Latence ajoutée par slow paths : ~500-1000ns

### Après Optimisations
- Lock acquisitions par packet : ~3-4 locks (réduction ~50%)
- Syscalls par refill : 0 (ioctl retiré)
- Latence ajoutée par slow paths : ~200-400ns (réduction ~60%)

## Conclusion

Les principaux slow paths sont liés aux **acquisitions multiples de locks** et aux **syscalls ioctl**. Les optimisations proposées devraient réduire la latence de ~30-50% dans le chemin critique.

