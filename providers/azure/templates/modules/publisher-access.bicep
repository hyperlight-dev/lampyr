param installationId string
param principalId string

var roles = [
  'acdd72a7-3385-48ef-bd42-f606fba81ae7'
  '428e0ff0-5e57-4d9c-a221-2c70d0e0a443'
]

resource assignments 'Microsoft.Authorization/roleAssignments@2022-04-01' = [for role in roles: {
  name: guid(resourceGroup().id, installationId, role, principalId)
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', role)
  }
}]

output roleAssignments array = [for (role, index) in roles: assignments[index].id]
