targetScope = 'subscription'

param name string
param location string
param installationId string

resource group 'Microsoft.Resources/resourceGroups@2024-03-01' = {
  name: name
  location: location
  tags: {
    'lampyr-installation-id': installationId
  }
}

output id string = group.id
