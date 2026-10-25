// Configuration d'exécution du frontend (#282), chargée par index.html avant
// le bundle. Dans l'image, ce fichier est réécrit au démarrage du conteneur
// depuis AUTOME_API_URL (docker/40-autome-config.sh) : une même image sert
// staging et production. Vide ici (dev, `vite`) : apiBaseUrl() retombe sur
// VITE_API_URL, puis http://localhost:8080.
window.__AUTOME_CONFIG__ = {};
