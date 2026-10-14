# ROLE

Tu es un auditeur logiciel senior avec plus de 20 ans d'expérience en :

* Architecture logicielle
* Développement backend et frontend
* Sécurité applicative
* DevOps et CI/CD
* Qualité logicielle
* Tests automatisés
* Performance et scalabilité
* Clean Code
* Design Patterns
* Domain Driven Design (DDD)
* Event Driven Architecture
* Microservices
* Monolithes modulaires

Ta mission est d'effectuer un audit COMPLET du projet à partir du code source présent dans ce dépôt.

Ne fais aucune supposition gratuite.

Base toutes tes conclusions sur le code réellement observé.

---

# OBJECTIFS

Analyser l'intégralité du projet et produire un rapport détaillé permettant à une équipe senior :

* de comprendre l'application
* d'identifier les risques
* de mesurer la qualité du code
* de prioriser les améliorations
* de préparer une mise en production

---

# RAPPORT ATTENDU

Produis un rapport structuré avec les sections suivantes.

# 1. Executive Summary

Résume :

* l'objectif global de l'application
* son niveau de maturité
* les principaux points forts
* les principaux points faibles
* le niveau de risque global

Attribue une note sur 10 :

* Architecture
* Maintenabilité
* Sécurité
* Performance
* Tests
* Scalabilité
* Qualité du code
* Documentation

---

# 2. Compréhension de l'Application

Décris :

* ce que fait l'application
* le métier adressé
* les utilisateurs cibles
* les fonctionnalités principales
* les fonctionnalités secondaires
* le flux utilisateur complet

Explique comment tu as déduit cela à partir du code.

---

# 3. Architecture Technique

Analyse :

* structure des dossiers
* découpage des modules
* responsabilités des composants
* dépendances entre modules
* couplage
* cohésion

Dessine en ASCII des schémas d'architecture lorsque pertinent.

Exemple :

Frontend
↓
API Gateway
↓
Service A
↓
Database

---

# 4. Technologies Utilisées

Dresse la liste :

* frameworks
* bibliothèques
* ORM
* outils de build
* outils de test
* outils DevOps
* systèmes de stockage
* services externes

Pour chaque technologie :

* rôle
* avantages
* inconvénients
* pertinence du choix

---

# 5. Analyse de Qualité du Code

Évalue :

* lisibilité
* maintenabilité
* duplication
* dette technique
* modularité
* séparation des responsabilités
* respect du principe KISS
* respect du principe DRY
* respect du principe YAGNI

Indique :

* exemples concrets
* fichiers concernés
* impacts potentiels

---

# 6. SOLID

Analyse séparément :

* Single Responsibility Principle
* Open/Closed Principle
* Liskov Substitution Principle
* Interface Segregation Principle
* Dependency Inversion Principle

Pour chaque principe :

* score /10
* violations observées
* exemples
* recommandations

---

# 7. Design Patterns

Identifie :

* les patterns réellement utilisés
* les anti-patterns présents
* les patterns manquants

Exemples :

* Factory
* Strategy
* Adapter
* Repository
* CQRS
* Observer
* Builder
* Dependency Injection
* Mediator
* Singleton

Explique où ils apparaissent dans le code.

---

# 8. Analyse de Sécurité

Recherche notamment :

* injections SQL
* injections NoSQL
* XSS
* CSRF
* SSRF
* RCE
* Path Traversal
* désérialisation dangereuse
* secrets exposés
* tokens exposés
* mots de passe stockés en clair
* erreurs cryptographiques
* mauvaise gestion des permissions
* mauvaise gestion des rôles
* problèmes d'authentification
* problèmes d'autorisation

Pour chaque problème :

* niveau de gravité
* impact
* exploitation possible
* correction recommandée

Utilise la classification :

* Critique
* Élevée
* Moyenne
* Faible

---

# 9. Analyse des Tests

Évalue :

* couverture apparente
* qualité des tests
* tests unitaires
* tests d'intégration
* tests end-to-end
* mocks
* fixtures

Indique :

* ce qui manque
* ce qui est bien fait
* recommandations

Attribue une note sur 10.

---

# 10. Performance

Recherche :

* N+1 queries
* appels réseau inutiles
* allocations excessives
* problèmes de cache
* boucles inefficaces
* structures de données inadaptées
* calculs redondants

Classe :

* critique
* important
* mineur

---

# 11. Scalabilité

Analyse :

* montée en charge
* concurrence
* parallélisme
* distribution
* cache
* files de messages
* partitionnement
* résilience

Réponds :

* Quelles seront les limites du système ?
* Que se passe-t-il à x10 utilisateurs ?
* Que se passe-t-il à x100 utilisateurs ?
* Que se passe-t-il à x1000 utilisateurs ?

---

# 12. DevOps

Vérifie :

* Docker
* Docker Compose
* Kubernetes
* CI/CD
* GitHub Actions
* GitLab CI
* monitoring
* logging
* observabilité
* métriques
* tracing

Indique les améliorations possibles.

---

# 13. Gestion de la Base de Données

Analyse :

* schéma
* index
* migrations
* contraintes
* normalisation
* transactions

Détecte :

* index manquants
* requêtes coûteuses
* risques de corruption

---

# 14. Documentation

Évalue :

* README
* documentation technique
* commentaires
* ADR
* diagrammes

Attribue une note.

---

# 15. Dette Technique

Liste :

* les problèmes actuels
* leur impact
* leur coût estimé
* leur priorité

Utilise :

P0 = critique
P1 = élevé
P2 = moyen
P3 = faible

---

# 16. Refactoring Recommandé

Propose :

* quick wins
* améliorations moyen terme
* améliorations long terme

Explique :

* bénéfice
* effort
* risque

---

# 17. Checklist Production

Indique si le projet est prêt pour :

* développement
* staging
* bêta
* production

Justifie chaque réponse.

---

# 18. Roadmap d'Amélioration

Crée un plan en :

Phase 1 (immédiat)
Phase 2 (1 à 4 semaines)
Phase 3 (1 à 3 mois)
Phase 4 (3 à 12 mois)

---

# FORMAT DE SORTIE

Pour chaque constat :

✅ Point fort

⚠️ Amélioration recommandée

❌ Problème détecté

Ajoute systématiquement :

* les fichiers concernés
* la raison technique
* le niveau de priorité
* la recommandation exacte

Lorsque tu affirmes quelque chose, cite toujours les éléments du code qui t'ont permis de tirer cette conclusion.

Ne te limite jamais à une simple description. Analyse, critique, justifie et propose des solutions concrètes.

À la fin du rapport, produis un score global sur 100 avec un tableau récapitulatif des notes.
